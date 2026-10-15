//! SDK session-only permission patches. Validate a whole batch before publishing.
use anyhow::{bail, ensure, Context, Result};
use claude_core::permissions::{rules::PermissionAction, PermissionManager};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Rule {
    tool_name: String,
    rule_content: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    #[serde(rename = "type")]
    operation: String,
    destination: String,
    behavior: String,
    rules: Vec<Rule>,
}
pub struct Updates {
    manager: Arc<PermissionManager>,
    session: String,
    rules: Mutex<Vec<HashMap<String, PermissionAction>>>,
}
impl Updates {
    pub fn new(manager: Arc<PermissionManager>, session: String) -> Self {
        Self {
            manager,
            session,
            rules: Mutex::new(vec![HashMap::new(); 3]),
        }
    }
    pub fn apply(&self, value: &Value) -> Result<()> {
        let updates: Vec<Update> =
            serde_json::from_value(value.clone()).context("Invalid SDK updatedPermissions")?;
        ensure!(updates.len() <= 256, "Too many SDK permission updates");
        let mut current = self.rules.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = current.clone();
        for update in updates {
            ensure!(
                update.destination == "session",
                "SDK permission updates only support destination session"
            );
            let (index, action) = match update.behavior.as_str() {
                "allow" => (0, PermissionAction::Allow),
                "ask" => (1, PermissionAction::Confirm),
                "deny" => (2, PermissionAction::Deny),
                _ => bail!("Unsupported SDK permission behavior"),
            };
            ensure!(update.rules.len() <= 1024, "Too many SDK permission rules");
            if update.operation == "replaceRules" {
                next[index].clear();
            }
            for rule in update.rules {
                ensure!(
                    !rule.tool_name.contains(['(', ')']),
                    "SDK toolName cannot contain a rule pattern"
                );
                let pattern = match rule.rule_content {
                    Some(content) => format!(
                        "{}({})",
                        rule.tool_name,
                        content
                            .replace('\\', "\\\\")
                            .replace('(', "\\(")
                            .replace(')', "\\)")
                    ),
                    None => rule.tool_name,
                };
                claude_core::permissions::patterns::validate(&pattern)?;
                match update.operation.as_str() {
                    "addRules" | "replaceRules" => { next[index].insert(pattern, action.clone()); },
                    "removeRules" => { next[index].remove(&pattern); },
                    _ => bail!("SDK permission updates support addRules, replaceRules and removeRules only"),
                }
            }
            ensure!(
                matches!(
                    update.operation.as_str(),
                    "addRules" | "replaceRules" | "removeRules"
                ),
                "Unsupported SDK permission update type"
            );
        }
        self.manager.set_runtime_rules(&self.session, next.clone());
        *current = next;
        Ok(())
    }
    pub fn clear(&self) {
        self.manager.clear_runtime_rules(&self.session);
    }
}
pub struct Cleanup(pub Arc<Updates>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_core::permissions::{
        AuditLogger, PermissionContext, PermissionMode, PermissionResult,
    };
    use serde_json::json;
    fn patch(operation: &str, behavior: &str) -> Value {
        json!({"type":operation,"destination":"session","behavior":behavior,"rules":[{"toolName":"Write"}]})
    }
    #[test]
    fn updates_are_atomic_scoped_and_behavior_specific() {
        let manager = Arc::new(PermissionManager::new(Arc::new(AuditLogger::new(16))));
        manager.set_mode(PermissionMode::AskPermissions);
        let updates = Arc::new(Updates::new(manager.clone(), "one".into()));
        let context = PermissionContext {
            tool_name: "Write".into(),
            tool_input: json!({"path":"test"}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        updates.apply(&json!([patch("addRules", "allow")])).unwrap();
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        for invalid in [
            json!([{"type":"setMode","destination":"session","mode":"bypassPermissions"}]),
            json!([{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write","ruleContent":"../outside"}]}]),
            json!([{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"LSP","ruleContent":"*"}]}]),
            json!([{"type":"addRules","destination":"session","behavior":"allow","rules":[],"unexpected":true}]),
        ] {
            assert!(updates.apply(&invalid).is_err());
            assert_eq!(manager.check_permission(&context), PermissionResult::Granted);
        }
        let other = PermissionContext {
            conversation_id: "two".into(),
            ..context.clone()
        };
        assert!(matches!(
            manager.check_permission(&other),
            PermissionResult::RequiresConfirmation(_)
        ));
        let mut invalid = patch("addRules", "deny");
        invalid["destination"] = json!("projectSettings");
        assert!(updates
            .apply(&json!([patch("removeRules", "allow"), invalid]))
            .is_err());
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        updates
            .apply(&json!([
                patch("addRules", "deny"),
                patch("addRules", "ask")
            ]))
            .unwrap();
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        updates
            .apply(&json!([patch("removeRules", "deny")]))
            .unwrap();
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
        updates.apply(&json!([{"type":"replaceRules","destination":"session","behavior":"ask","rules":[]}])).unwrap();
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
        manager.set_session_rules(
            "one",
            HashMap::from([("Write".into(), PermissionAction::Deny)]),
        );
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        manager.set_session_rules(
            "one",
            HashMap::from([("Write".into(), PermissionAction::Confirm)]),
        );
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
        manager.set_session_rules("one", HashMap::new());
        drop(Cleanup(updates));
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
    }
}
