//! SDK session-only permission patches. Validate a whole batch before publishing.
use crate::permissions::{rules::PermissionAction, PermissionManager};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

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
}
impl Updates {
    pub fn new(manager: Arc<PermissionManager>, session: String) -> Self {
        Self { manager, session }
    }
    pub fn apply(&self, value: &Value) -> Result<()> {
        self.apply_inner(value, true)
    }
    pub fn prepare(
        self: &Arc<Self>,
        value: &Value,
    ) -> Result<crate::native_engine::permission_response::Commit> {
        self.apply_inner(value, false)?;
        let updates = self.clone();
        let value = value.clone();
        Ok(Box::new(move || updates.apply(&value)))
    }
    fn apply_inner(&self, value: &Value, publish: bool) -> Result<()> {
        let updates: Vec<Update> =
            serde_json::from_value(value.clone()).context("Invalid SDK updatedPermissions")?;
        ensure!(updates.len() <= 256, "Too many SDK permission updates");
        self.manager.update_runtime_rules(&self.session, publish, |next| {
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
                crate::permissions::patterns::validate(&pattern)?;
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
        Ok(())
        })
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
    use std::collections::HashMap;
    use crate::permissions::{AuditLogger, PermissionContext, PermissionMode, PermissionResult};
    use serde_json::json;
    fn patch(operation: &str, behavior: &str) -> Value {
        json!({"type":operation,"destination":"session","behavior":behavior,"rules":[{"toolName":"Write"}]})
    }
    #[test]
    fn independently_prepared_patches_preserve_later_rules() {
        let manager = Arc::new(PermissionManager::new(Arc::new(AuditLogger::new(16))));
        let updates = Arc::new(Updates::new(manager.clone(), "one".into()));
        let allow = updates
            .prepare(&json!([patch("addRules", "allow")]))
            .unwrap();
        let deny = updates
            .prepare(&json!([patch("addRules", "deny")]))
            .unwrap();
        allow().unwrap();
        deny().unwrap();
        let context = PermissionContext {
            tool_name: "Write".into(),
            tool_input: json!({}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::Denied(_)
        ));
        updates
            .apply(&json!([patch("removeRules", "deny")]))
            .unwrap();
        assert_eq!(
            manager.check_permission(&context),
            PermissionResult::Granted
        );
    }
    #[test]
    fn child_with_no_inherited_runtime_rules_can_prepare_a_patch() {
        let parent = PermissionManager::new(Arc::new(AuditLogger::new(16)));
        let child = Arc::new(parent.fork_session("parent", "child"));
        let updates = Arc::new(Updates::new(child.clone(), "child".into()));
        updates
            .prepare(&json!([patch("addRules", "allow")]))
            .unwrap()()
        .unwrap();
        assert_eq!(
            child.check_permission(&PermissionContext {
                tool_name: "Write".into(),
                tool_input: json!({}),
                conversation_id: "child".into(),
                user_id: None,
                workspace_path: None
            }),
            PermissionResult::Granted
        );
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
            assert_eq!(
                manager.check_permission(&context),
                PermissionResult::Granted
            );
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
