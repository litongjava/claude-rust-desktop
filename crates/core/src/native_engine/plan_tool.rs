use super::*;
use crate::{permissions::PermissionMode, services::plans};

impl ToolLoopExecutor {
    pub(super) async fn execute_plan_transition(
        &mut self,
        tool_name: &str,
        input: &Value,
        id: &str,
    ) -> (Value, String, bool) {
        let result = self.plan_transition(tool_name, input, id).await;
        let failed = result.is_err();
        (
            input.clone(),
            match result {
                Ok(value) => value.to_string(),
                Err(error) => format!("Plan mode: {error:#}"),
            },
            failed,
        )
    }

    async fn plan_transition(&mut self, tool_name: &str, input: &Value, id: &str) -> Result<Value> {
        let entering = tool_name == "EnterPlanMode"
            || (tool_name == "plan_mode" && input["operation"] == "enter");
        match self.check_permission(tool_name, input).await {
            PermissionResult::Denied(reason) => anyhow::bail!("{reason}"),
            PermissionResult::RequiresConfirmation(_) if entering => {
                let request = uuid::Uuid::new_v4().to_string();
                let (tx, rx) = oneshot::channel();
                self.answer_waiters.lock().await.insert(request.clone(), tx);
                if self
                    .event_tx
                    .send(EngineEvent::ToolPermission {
                        request_id: request.clone(),
                        tool_use_id: id.into(),
                        tool_name: tool_name.into(),
                        input: input.clone(),
                    })
                    .await
                    .is_err()
                {
                    self.answer_waiters.lock().await.remove(&request);
                    anyhow::bail!("Approval host disconnected");
                }
                let answer = self.wait_for_user_answer(rx).await;
                self.answer_waiters.lock().await.remove(&request);
                anyhow::ensure!(
                    answer.ok().and_then(|a| a.ok()).is_some_and(|a| matches!(
                        a.trim().to_lowercase().as_str(),
                        "yes" | "y" | "allow" | "approved" | "允许" | "是"
                    )),
                    "Entering plan mode was not approved"
                );
            }
            _ => {}
        }
        let manager = self
            .permission_manager
            .clone()
            .ok_or_else(|| anyhow!("Permission manager unavailable"))?;
        let root = self
            .search_config_path
            .as_deref()
            .and_then(std::path::Path::parent)
            .ok_or_else(|| anyhow!("Plan storage unavailable"))?
            .to_path_buf();
        let session = self
            .conv_id
            .clone()
            .ok_or_else(|| anyhow!("Plan session unavailable"))?;
        let path = plans::path(&root, &session)?;
        if entering {
            if tool_name == "plan_mode" {
                let mut plan = crate::tools::coordination::execute(
                    tool_name,
                    input.clone(),
                    &self.workspace_cwd,
                )?;
                plan["mode"] = json!("plan");
                plan["planFilePath"] = json!(path);
                manager.set_mode(PermissionMode::PlanMode);
                return Ok(plan);
            }
            manager.set_mode(PermissionMode::PlanMode);
            return Ok(
                json!({"mode":"plan","planFilePath":path,"plan":plans::read(&root,&session)?,
                "message":"Read-only planning enabled. Submit Markdown with ExitPlanMode(plan); do not modify project files."}),
            );
        }
        anyhow::ensure!(
            manager.get_mode() == PermissionMode::PlanMode,
            "Enter plan mode before requesting plan approval"
        );
        if let Some(plan) = input.get("plan") {
            plans::save(
                &root,
                &session,
                plan.as_str()
                    .ok_or_else(|| anyhow!("plan must be Markdown text"))?,
            )?;
        } else if tool_name == "plan_mode" {
            let current = crate::tools::coordination::execute(
                "plan_mode",
                json!({"operation":"get_current"}),
                &self.workspace_cwd,
            )?;
            if !current.is_null() {
                plans::save(
                    &root,
                    &session,
                    &format!(
                        "# Implementation plan\n\n```json\n{}\n```\n",
                        serde_json::to_string_pretty(&current)?
                    ),
                )?;
            }
        }
        let plan = plans::read(&root, &session)?
            .filter(|plan| !plan.trim().is_empty())
            .ok_or_else(|| anyhow!("No plan written yet; supply ExitPlanMode(plan)"))?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.answer_waiters
            .lock()
            .await
            .insert(request_id.clone(), tx);
        let sent = self
            .event_tx
            .send(EngineEvent::ToolPermission {
                request_id: request_id.clone(),
                tool_use_id: id.into(),
                tool_name: "ExitPlanMode".into(),
                input: json!({"plan":plan,"planFilePath":path,"permissionMode":"ask"}),
            })
            .await;
        if sent.is_err() {
            self.answer_waiters.lock().await.remove(&request_id);
            anyhow::bail!("Approval host disconnected; planning mode retained");
        }
        let answer = self.wait_for_user_answer(rx).await;
        self.answer_waiters.lock().await.remove(&request_id);
        let approved = answer
            .ok()
            .and_then(|answer| answer.ok())
            .is_some_and(|answer| {
                matches!(
                    answer.trim().to_lowercase().as_str(),
                    "yes" | "y" | "allow" | "approved" | "允许" | "是"
                )
            });
        anyhow::ensure!(approved, "Plan was not approved; planning mode retained");
        anyhow::ensure!(
            plans::read(&root, &session)?.as_deref() == Some(plan.as_str()),
            "Plan changed during review; request approval again"
        );
        anyhow::ensure!(
            manager.get_mode() == PermissionMode::PlanMode,
            "Permission mode changed during review; approval was not applied"
        );
        if tool_name == "plan_mode" {
            crate::tools::coordination::execute(
                "plan_mode",
                json!({"operation":"exit"}),
                &self.workspace_cwd,
            )?;
        }
        manager.set_mode(PermissionMode::AskPermissions);
        Ok(
            json!({"approved":true,"mode":"ask","planFilePath":path,"plan":plan,
            "message":"Plan approved. Implementation may proceed under normal ask permissions."}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enter_plan_confirmation_and_child_authority_are_isolated() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        for name in ["EnterPlanMode", "plan_mode"] {
            for answer in [Some("yes"), Some("no"), None] {
                let root = tempfile::tempdir().unwrap();
                let parent = crate::runtime::default_permissions();
                parent.set_mode(PermissionMode::BypassPermissions);
                parent.set_session_rules(
                    "parent",
                    std::collections::HashMap::from([
                        (
                            name.into(),
                            crate::permissions::rules::PermissionAction::Confirm,
                        ),
                        (
                            "Write".into(),
                            crate::permissions::rules::PermissionAction::Deny,
                        ),
                    ]),
                );
                let child = Arc::new(parent.fork_session("parent", "child"));
                let provider = ResolvedProvider {
                provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
                model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
            };
                let (tx, mut events) = mpsc::channel(16);
                let mut executor = ToolLoopExecutor::new(
                    provider,
                    vec![],
                    None,
                    128,
                    tx,
                    root.path().to_string_lossy().into_owned(),
                    ReasoningMode::Standard,
                )
                .with_conv_id("child".into())
                .with_search_config_path(root.path().join("settings.json"))
                .with_permission_manager(child.clone());
                let input = if name == "plan_mode" {
                    let plan = crate::tools::coordination::execute("plan_mode", json!({"operation":"create","task":"legacy test","steps":[{"description":"Inspect"}]}), &executor.workspace_cwd).unwrap();
                    json!({"operation":"enter","plan_id":plan["plan_id"]})
                } else {
                    json!({})
                };
                let waiters = executor.get_answer_waiters();
                let task = tokio::spawn(async move {
                    executor
                        .execute_tool_call(name, &input, "enter-child")
                        .await
                });
                let EngineEvent::ToolPermission {
                    request_id,
                    tool_name,
                    ..
                } = events.recv().await.unwrap()
                else {
                    panic!("Explicit confirmation was bypassed");
                };
                assert_eq!(tool_name, name);
                assert_eq!(child.get_mode(), PermissionMode::BypassPermissions);
                let sender = waiters.lock().await.remove(&request_id).unwrap();
                if let Some(answer) = answer {
                    sender.send(answer.into()).unwrap();
                } else {
                    drop(sender);
                }
                let (_, output, failed) = task.await.unwrap();
                assert_eq!(failed, answer != Some("yes"), "{output}");
                if name == "plan_mode" {
                    let current = crate::tools::coordination::execute(
                        "plan_mode",
                        json!({"operation":"get_current"}),
                        &root.path().to_string_lossy(),
                    )
                    .unwrap();
                    assert_eq!(current.is_null(), failed);
                }
                assert_eq!(parent.get_mode(), PermissionMode::BypassPermissions);
                assert_eq!(
                    child.get_mode(),
                    if failed {
                        PermissionMode::BypassPermissions
                    } else {
                        PermissionMode::PlanMode
                    }
                );
                child.set_mode(PermissionMode::BypassPermissions);
                let context = crate::permissions::PermissionContext {
                    tool_name: "Write".into(),
                    tool_input: json!({}),
                    conversation_id: "child".into(),
                    user_id: None,
                    workspace_path: None,
                };
                assert!(matches!(
                    child.check_permission(&context),
                    PermissionResult::Denied(_)
                ));
                child.set_session_rules("child", Default::default());
                let context = crate::permissions::PermissionContext {
                    conversation_id: "parent".into(),
                    ..context
                };
                assert!(matches!(
                    parent.check_permission(&context),
                    PermissionResult::Denied(_)
                ));
                assert!(waiters.lock().await.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn plan_approval_requires_explicit_answer_and_preserves_read_only_on_failure() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        for answer in [
            Some("yes"),
            Some("no"),
            None,
            Some("changed"),
            Some("always_allow"),
            Some("legacy"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let manager = crate::runtime::default_permissions();
            manager.set_mode(PermissionMode::BypassPermissions);
            let provider = ResolvedProvider {
                provider: serde_json::from_value(json!({"id":"plan-test","name":"plan-test","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
                model: serde_json::from_value(json!({"id":"plan-test","name":"plan-test","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
            };
            let (tx, mut rx) = mpsc::channel(16);
            let mut executor = ToolLoopExecutor::new(
                provider,
                vec![],
                None,
                128,
                tx,
                root.path().to_string_lossy().into_owned(),
                ReasoningMode::Standard,
            )
            .with_conv_id("plan-session".into())
            .with_search_config_path(root.path().join("settings.json"))
            .with_permission_manager(manager.clone());
            let (_, output, failed) = executor
                .execute_tool_call("EnterPlanMode", &json!({}), "enter")
                .await;
            assert!(!failed, "{output}");
            assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
            let (_, _, failed) = executor
                .execute_tool_call(
                    "Write",
                    &json!({"file_path":"blocked.txt","content":"no"}),
                    "write",
                )
                .await;
            assert!(failed);
            assert!(!root.path().join("blocked.txt").exists());
            let (_, _, failed) = executor
                .execute_tool_call("ExitPlanMode", &json!({}), "empty")
                .await;
            assert!(failed);
            manager.set_session_rules(
                "plan-session",
                std::collections::HashMap::from([(
                    "ExitPlanMode".into(),
                    crate::permissions::rules::PermissionAction::Deny,
                )]),
            );
            let (_, _, denied) = executor
                .execute_tool_call("ExitPlanMode", &json!({"plan":"Denied"}), "denied")
                .await;
            assert!(denied);
            assert!(rx.try_recv().is_err());
            manager.set_session_rules("plan-session", Default::default());
            if answer == Some("legacy") {
                let plan = crate::tools::coordination::execute("plan_mode", json!({"operation":"create","task":"修复","steps":[{"description":"Run tests"}]}), &executor.workspace_cwd).unwrap();
                executor
                    .execute_tool_call(
                        "plan_mode",
                        &json!({"operation":"enter","plan_id":plan["plan_id"]}),
                        "legacy-enter",
                    )
                    .await;
            }
            let waiters = executor.get_answer_waiters();
            let task = tokio::spawn(async move {
                if answer == Some("legacy") {
                    return executor
                        .execute_tool_call("plan_mode", &json!({"operation":"exit"}), "exit")
                        .await;
                }
                executor
                    .execute_tool_call(
                        "ExitPlanMode",
                        &json!({"plan":"# Plan\n1. 修复\n2. Run regression tests"}),
                        "exit",
                    )
                    .await
            });
            let event = timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let EngineEvent::ToolPermission {
                request_id,
                tool_use_id,
                tool_name,
                input,
            } = event
            else {
                panic!("Expected plan approval");
            };
            assert_eq!(tool_use_id, "exit");
            assert_eq!(tool_name, "ExitPlanMode");
            assert!(input["plan"].as_str().unwrap().contains("修复"));
            assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
            if answer == Some("changed") {
                plans::save(root.path(), "plan-session", "# Changed during review").unwrap();
            }
            let sender = waiters.lock().await.remove(&request_id).unwrap();
            if let Some(answer) = answer {
                sender
                    .send(
                        if matches!(answer, "changed" | "legacy") {
                            "yes"
                        } else {
                            answer
                        }
                        .into(),
                    )
                    .unwrap();
            } else {
                drop(sender);
            }
            let (_, output, failed) = timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                failed,
                !matches!(answer, Some("yes" | "legacy")),
                "{output}"
            );
            assert_eq!(
                manager.get_mode(),
                if failed {
                    PermissionMode::PlanMode
                } else {
                    PermissionMode::AskPermissions
                }
            );
            assert!(waiters.lock().await.is_empty());
            assert!(plans::read(root.path(), "plan-session").unwrap().is_some());
        }
    }
}
