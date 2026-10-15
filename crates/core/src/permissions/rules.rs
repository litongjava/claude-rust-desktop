use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PermissionAction {
    Allow,
    Deny,
    Confirm,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PermissionScope {
    Global,
    Workspace,
    Conversation,
    ToolSpecific,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum PermissionLevel {
    None,
    Read,
    Write,
    Execute,
    Admin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub id: String,
    pub tool_name: Option<String>,
    pub action: PermissionAction,
    pub scope: PermissionScope,
    pub level: PermissionLevel,
    pub conditions: Option<serde_json::Value>,
    pub description: Option<String>,
    pub priority: i32,
}

impl PermissionRule {
    pub fn new(id: &str, action: PermissionAction, scope: PermissionScope, level: PermissionLevel) -> Self {
        Self {
            id: id.to_string(),
            tool_name: None,
            action,
            scope,
            level,
            conditions: None,
            description: None,
            priority: 0,
        }
    }

    pub fn with_tool_name(mut self, tool_name: &str) -> Self {
        self.tool_name = Some(tool_name.to_string());
        self
    }

    pub fn with_conditions(mut self, conditions: serde_json::Value) -> Self {
        self.conditions = Some(conditions);
        self
    }

    pub fn with_description(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRuleset {
    pub rules: Vec<PermissionRule>,
}

impl PermissionRuleset {
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    pub fn add_rule(&mut self, rule: PermissionRule) {
        self.rules.push(rule);
        self.rules.sort_by(|a, b| b.priority.cmp(&a.priority));
    }

    pub fn get_rules_for_tool(&self, tool_name: &str) -> Vec<PermissionRule> {
        self.rules
            .iter()
            .filter(|rule| {
                rule.tool_name.as_deref() == Some(tool_name) || rule.tool_name.is_none()
            })
            .cloned()
            .collect()
    }
}

// Default operation policy is evaluated by PermissionManager. This ruleset
// represents explicit overrides; a global Confirm rule would incorrectly match
// ordinary reads before their default read-only allowance.

pub const DANGEROUS_TOOLS: &[&str] = &["bash", "command", "computer_use", "file_write", "file_delete", "FileDelete", "ProcessKill", "LSP", "lsp", "worktree"];
