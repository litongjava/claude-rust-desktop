use super::audit::{AuditAction, AuditEntry, AuditLoggerRef};
use super::rules::{DANGEROUS_TOOLS, PermissionAction, PermissionRule, PermissionRuleset};
use super::{PermissionChecker, PermissionContext, PermissionResult, ToolPermission};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    AskPermissions,
    AcceptEdits,
    PlanMode,
    BypassPermissions,
}

impl Default for PermissionMode {
    fn default() -> Self {
        PermissionMode::AcceptEdits
    }
}

impl PermissionMode {
    pub fn from_str(s: &str) -> Self {
        let s_lower = s.to_lowercase();
        match s_lower.as_str() {
            "ask_permissions" | "ask" | "ask_permission" => PermissionMode::AskPermissions,
            "accept_edits" | "edits" => PermissionMode::AcceptEdits,
            "plan_mode" | "plan" => PermissionMode::PlanMode,
            "bypass_permissions" | "bypass" | "always_allow" | "alwaysallow" | "zh" => PermissionMode::BypassPermissions,
            _ => PermissionMode::AcceptEdits,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionMode::AskPermissions => "ask_permissions",
            PermissionMode::AcceptEdits => "accept_edits",
            PermissionMode::PlanMode => "plan_mode",
            PermissionMode::BypassPermissions => "bypass_permissions",
        }
    }
}

pub type AlwaysAllowCheckerFn = Box<dyn Fn(&str, &str) -> bool + Send + Sync>;

pub struct PermissionManager {
    ruleset: Mutex<PermissionRuleset>,
    tool_permissions: Mutex<HashMap<String, ToolPermission>>,
    audit_logger: AuditLoggerRef,
    confirmations: Mutex<HashMap<String, bool>>,
    mode: Mutex<PermissionMode>,
    session_rules: Mutex<HashMap<String, HashMap<String, PermissionAction>>>,
    runtime_rules: Mutex<HashMap<String, Vec<HashMap<String, PermissionAction>>>>,
    always_allow_checker: Mutex<Option<Arc<dyn Fn(&str, &str) -> bool + Send + Sync>>>,
}

impl PermissionManager {
    /// Ephemeral host rules, independent of persisted session configuration.
    pub fn set_runtime_rules(&self, conversation: &str, rules: Vec<HashMap<String, PermissionAction>>) {
        self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner()).insert(conversation.to_owned(), rules);
    }
    pub fn clear_runtime_rules(&self, conversation: &str) {
        self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner()).remove(conversation);
    }
    pub fn set_session_rules(&self, conversation: &str, rules: HashMap<String, PermissionAction>) {
        self.session_rules.lock().unwrap_or_else(|e| e.into_inner()).insert(conversation.to_owned(), rules);
    }
    pub(crate) fn inherit_session_rules(&self, parent: &str, child: &str) {
        let mut sessions = self.session_rules.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rules) = sessions.get(parent).cloned() { sessions.insert(child.to_owned(), rules); }
        let mut runtime = self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rules) = runtime.get(parent).cloned() { runtime.insert(child.to_owned(), rules); }
    }

    /// Independent mutable authority for a delegated session; read-only policy
    /// callback and audit sink may be shared, but mode/approvals/rules may not.
    pub(crate) fn fork_session(&self, parent: &str, child: &str) -> Self {
        let rules = self.session_rules.lock().unwrap_or_else(|e| e.into_inner())
            .get(parent).cloned().unwrap_or_default();
        Self {
            ruleset: Mutex::new(self.ruleset.lock().unwrap_or_else(|e| e.into_inner()).clone()),
            tool_permissions: Mutex::new(self.tool_permissions.lock().unwrap_or_else(|e| e.into_inner()).clone()),
            audit_logger: self.audit_logger.clone(),
            confirmations: Mutex::new(HashMap::new()),
            mode: Mutex::new(self.get_mode()),
            session_rules: Mutex::new(HashMap::from([(child.to_owned(), rules)])),
            runtime_rules: Mutex::new(HashMap::from([(child.to_owned(), self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner()).get(parent).cloned().unwrap_or_default())])),
            always_allow_checker: Mutex::new(self.always_allow_checker.lock().unwrap_or_else(|e| e.into_inner()).clone()),
        }
    }

    pub fn new(audit_logger: AuditLoggerRef) -> Self {
        let mut manager = Self {
            ruleset: Mutex::new(PermissionRuleset::new()),
            tool_permissions: Mutex::new(HashMap::new()),
            audit_logger,
            confirmations: Mutex::new(HashMap::new()),
            mode: Mutex::new(PermissionMode::default()),
            session_rules: Mutex::new(HashMap::new()),
            runtime_rules: Mutex::new(HashMap::new()),
            always_allow_checker: Mutex::new(None),
        };
        manager.load_default_rules();
        manager
    }

    pub fn set_always_allow_checker(&self, checker: AlwaysAllowCheckerFn) {
        let mut guard = self.always_allow_checker.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Arc::from(checker));
        tracing::info!(module = "Permission", "Always allow checker registered");
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        let mut m = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        *m = mode;
        tracing::info!(module = "Permission", "Mode changed to: {}", mode.as_str());
    }

    pub fn get_mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn load_default_rules(&mut self) {
        let default_rules = super::rules::get_default_rules();
        let mut ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        *ruleset = default_rules;
    }

    pub fn add_rule(&self, rule: PermissionRule) {
        let mut ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        ruleset.add_rule(rule);
    }

    pub fn remove_rule(&self, rule_id: &str) {
        let mut ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        ruleset.rules.retain(|r| r.id != rule_id);
    }

    pub fn get_rules(&self) -> Vec<PermissionRule> {
        let ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        ruleset.rules.clone()
    }

    pub fn set_tool_permission(&self, tool_name: &str, permission: ToolPermission) {
        let mut permissions = self.tool_permissions.lock().unwrap_or_else(|e| e.into_inner());
        permissions.insert(tool_name.to_string(), permission);
    }

    pub fn get_tool_permission(&self, tool_name: &str) -> Option<ToolPermission> {
        let permissions = self.tool_permissions.lock().unwrap_or_else(|e| e.into_inner());
        permissions.get(tool_name).cloned()
    }

    fn is_read_only_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "Read" | "ListDir" | "Grep" | "Glob" | "WebFetch" | "WebSearch"
                | "TodoRead" | "Skill" | "SkillRead" | "TaskGet" | "TaskList" | "TaskOutput" | "ToolSearch" | "plan_mode" | "EnterPlanMode" | "ExitPlanMode" | "brief" | "SyntheticOutput"
        )
    }

    fn is_edit_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "Write" | "Edit" | "SearchReplace" | "GlobReplace" | "Delete"
                | "SkillCreate" | "SkillUpdate" | "NotebookEdit" | "TaskCreate" | "TaskUpdate" | "TaskStop" | "todo_write" | "TodoWrite" | "SendMessage" | "team" | "cron" | "Config"
        )
    }

    fn is_dangerous_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "Bash" | "PluginRun" | "LSP" | "lsp" | "worktree" | "Delete" | "FileDelete" | "ProcessKill"
        )
    }

    pub fn check_permission(&self, context: &PermissionContext) -> PermissionResult {
        let explicit = self.session_rules.lock().unwrap_or_else(|e| e.into_inner())
            .get(&context.conversation_id).and_then(|rules| super::patterns::matching_action(rules, context));
        let runtime = self.runtime_rules.lock().unwrap_or_else(|e| e.into_inner());
        let explicit = runtime.get(&context.conversation_id).into_iter().flatten()
            .filter_map(|rules| super::patterns::matching_action(rules, context))
            .chain(explicit)
            .max_by_key(|action| match action { PermissionAction::Deny => 3, PermissionAction::Confirm => 2, PermissionAction::Allow => 1 });
        drop(runtime);
        if explicit == Some(PermissionAction::Deny) {
            return PermissionResult::Denied("Explicit session tool deny rule".into());
        }
        if explicit == Some(PermissionAction::Confirm)
            && (self.get_mode() != PermissionMode::PlanMode || Self::is_read_only_tool(&context.tool_name))
        {
            return self.request_confirmation(context);
        }
        if self.get_mode() != PermissionMode::PlanMode {
            match explicit {
                Some(PermissionAction::Allow) => return PermissionResult::Granted,
                _ => {},
            }
        }
        let mode = self.get_mode();
        let read_only = Self::is_read_only_tool(&context.tool_name)
            || (context.tool_name == "Config" && matches!(context.tool_input["operation"].as_str(), Some("get" | "list" | "merge" | "sources")));

        // Bypass permissions: all operations auto-approved
        if mode == PermissionMode::BypassPermissions {
            tracing::info!(module = "Permission", "Bypass mode: {} auto-approved", context.tool_name);
            self.audit_logger.log(
                AuditEntry::new(AuditAction::PermissionGranted, &context.tool_name, &context.conversation_id)
                    .with_user_id(context.user_id.as_deref())
                    .with_result("Bypass mode"),
            );
            return PermissionResult::Granted;
        }

        // Plan mode: only read-only operations allowed
        if mode == PermissionMode::PlanMode {
            if read_only {
                tracing::info!(module = "Permission", "Plan mode: {} auto-approved (read-only)", context.tool_name);
                self.audit_logger.log(
                    AuditEntry::new(AuditAction::PermissionGranted, &context.tool_name, &context.conversation_id)
                        .with_user_id(context.user_id.as_deref())
                        .with_result("Plan mode: read-only"),
                );
                return PermissionResult::Granted;
            } else {
                let reason = "Plan mode: write operations are not allowed";
                tracing::warn!(module = "Permission", "Plan mode: {} denied", context.tool_name);
                self.audit_logger.log(
                    AuditEntry::new(AuditAction::PermissionDenied, &context.tool_name, &context.conversation_id)
                        .with_user_id(context.user_id.as_deref())
                        .with_result(reason),
                );
                return PermissionResult::Denied(reason.to_string());
            }
        }

        // Always allow check: if tool/action is in the always allow list, auto-approve
        if let Some(ref checker) = *self.always_allow_checker.lock().unwrap_or_else(|e| e.into_inner()) {
            let action = self.extract_action_from_context(context);
            if checker(&context.tool_name, &action) {
                tracing::info!(module = "Permission", "Always allow: {} ({}) auto-approved", context.tool_name, action);
                self.audit_logger.log(
                    AuditEntry::new(AuditAction::PermissionGranted, &context.tool_name, &context.conversation_id)
                        .with_user_id(context.user_id.as_deref())
                        .with_result("Always allow rule matched"),
                );
                return PermissionResult::Granted;
            }
        }

        // Accept edits mode: read and edit operations auto-approved, dangerous operations need confirmation
        if mode == PermissionMode::AcceptEdits {
            if read_only || Self::is_edit_tool(&context.tool_name) {
                tracing::info!(module = "Permission", "Accept edits mode: {} auto-approved", context.tool_name);
                self.audit_logger.log(
                    AuditEntry::new(AuditAction::PermissionGranted, &context.tool_name, &context.conversation_id)
                        .with_user_id(context.user_id.as_deref())
                        .with_result("Accept edits mode"),
                );
                return PermissionResult::Granted;
            }
            // Dangerous tools still need confirmation in accept_edits mode
            if Self::is_dangerous_tool(&context.tool_name) {
                tracing::warn!(module = "Permission", "Accept edits mode: {} requires confirmation (dangerous)", context.tool_name);
                return self.request_confirmation(context);
            }
        }

        // Ask permissions mode (default): fall through to existing logic
        self.audit_logger.log(
            AuditEntry::new(AuditAction::PermissionChecked, &context.tool_name, &context.conversation_id)
                .with_user_id(context.user_id.as_deref()),
        );

        let ruleset = self.ruleset.lock().unwrap_or_else(|e| e.into_inner());
        let rules = ruleset.get_rules_for_tool(&context.tool_name);
        
        for rule in rules {
            let result = self.evaluate_rule(&rule, context);
            if result != PermissionResult::Granted {
                return result;
            }
        }
        drop(ruleset);

        if DANGEROUS_TOOLS.contains(&context.tool_name.as_str()) {
            return self.handle_dangerous_tool(context);
        }

        let tool_permission = self.get_tool_permission(&context.tool_name);
        if let Some(perm) = tool_permission {
            if !perm.allowed {
                let result = PermissionResult::Denied(format!("Tool {} is not allowed", context.tool_name));
                self.audit_logger.log(
                    AuditEntry::new(AuditAction::PermissionDenied, &context.tool_name, &context.conversation_id)
                        .with_user_id(context.user_id.as_deref())
                        .with_result("Tool not allowed"),
                );
                return result;
            }
            if perm.requires_confirmation {
                return self.request_confirmation(context);
            }
        }

        self.audit_logger.log(
            AuditEntry::new(AuditAction::PermissionGranted, &context.tool_name, &context.conversation_id)
                .with_user_id(context.user_id.as_deref())
                .with_result("Granted"),
        );
        PermissionResult::Granted
    }

    fn extract_action_from_context(&self, context: &PermissionContext) -> String {
        if let Some(cmd) = context.tool_input.get("command").and_then(|v| v.as_str()) {
            format!("command:{}", cmd.split_whitespace().next().unwrap_or(""))
        } else if let Some(path) = context.tool_input.get("path").and_then(|v| v.as_str()) {
            format!("path:{}", path)
        } else if let Some(content) = context.tool_input.get("content").and_then(|v| v.as_str()) {
            if content.len() > 50 {
                format!("content:{}", &content[..50])
            } else {
                format!("content:{}", content)
            }
        } else {
            "unknown".to_string()
        }
    }

    fn evaluate_rule(&self, rule: &PermissionRule, context: &PermissionContext) -> PermissionResult {
        if let Some(tool_name) = &rule.tool_name {
            if tool_name != &context.tool_name {
                return PermissionResult::Granted;
            }
        }

        match rule.action {
            PermissionAction::Allow => PermissionResult::Granted,
            PermissionAction::Deny => {
                let reason = format!("Rule {} denies access", rule.id);
                self.audit_logger.log(
                    AuditEntry::new(AuditAction::PermissionDenied, &context.tool_name, &context.conversation_id)
                        .with_user_id(context.user_id.as_deref())
                        .with_result(&reason),
                );
                PermissionResult::Denied(reason)
            }
            PermissionAction::Confirm => {
                self.request_confirmation(context)
            }
        }
    }

    fn handle_dangerous_tool(&self, context: &PermissionContext) -> PermissionResult {
        let confirm_key = format!("{}-{}", context.conversation_id, context.tool_name);
        let confirmations = self.confirmations.lock().unwrap_or_else(|e| e.into_inner());
        
        if confirmations.get(&confirm_key) == Some(&true) {
            PermissionResult::Granted
        } else {
            self.request_confirmation(context)
        }
    }

    fn request_confirmation(&self, context: &PermissionContext) -> PermissionResult {
        let message = format!(
            "Tool '{}' requires user confirmation before execution",
            context.tool_name
        );
        PermissionResult::RequiresConfirmation(message)
    }

    pub fn confirm_permission(&self, conversation_id: &str, tool_name: &str) {
        let confirm_key = format!("{}-{}", conversation_id, tool_name);
        let mut confirmations = self.confirmations.lock().unwrap_or_else(|e| e.into_inner());
        confirmations.insert(confirm_key, true);

        self.audit_logger.log(
            AuditEntry::new(AuditAction::PermissionConfirmed, tool_name, conversation_id)
                .with_result("User confirmed"),
        );
    }

    pub fn clear_confirmation(&self, conversation_id: &str, tool_name: &str) {
        let confirm_key = format!("{}-{}", conversation_id, tool_name);
        let mut confirmations = self.confirmations.lock().unwrap_or_else(|e| e.into_inner());
        confirmations.remove(&confirm_key);
    }

    pub fn clear_all_confirmations(&self) {
        let mut confirmations = self.confirmations.lock().unwrap_or_else(|e| e.into_inner());
        confirmations.clear();
    }

    pub fn log_tool_execution(&self, context: &PermissionContext, success: bool) {
        let action = if success { AuditAction::ToolExecuted } else { AuditAction::ToolCancelled };
        let result = if success { "Success" } else { "Cancelled" };
        
        self.audit_logger.log(
            AuditEntry::new(action, &context.tool_name, &context.conversation_id)
                .with_user_id(context.user_id.as_deref())
                .with_result(result)
                .with_details(context.tool_input.clone()),
        );
    }

    pub fn is_tool_dangerous(&self, tool_name: &str) -> bool {
        DANGEROUS_TOOLS.contains(&tool_name)
    }
}

impl PermissionChecker for PermissionManager {
    fn check_permission(&self, context: &PermissionContext) -> PermissionResult {
        self.check_permission(context)
    }
}

pub type PermissionManagerRef = Arc<PermissionManager>;

#[cfg(test)]
mod session_rule_tests {
    use super::*;

    #[test]
    fn runtime_rules_fork_independently_and_preserve_plan_mode() {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        manager.set_mode(PermissionMode::AskPermissions);
        manager.set_runtime_rules("parent", vec![HashMap::from([("Write".into(), PermissionAction::Allow)])]);
        let child = manager.fork_session("parent", "child");
        manager.clear_runtime_rules("parent");
        let context = PermissionContext { tool_name:"Write".into(), tool_input:serde_json::json!({"path":"test"}), conversation_id:"child".into(), user_id:None, workspace_path:None };
        assert_eq!(child.check_permission(&context), PermissionResult::Granted);
        child.set_mode(PermissionMode::PlanMode);
        assert!(matches!(child.check_permission(&context), PermissionResult::Denied(_)));
        assert!(matches!(manager.check_permission(&PermissionContext { conversation_id:"parent".into(), ..context }), PermissionResult::RequiresConfirmation(_)));
    }

    #[test]
    fn pattern_rules_enforce_denies_and_complex_shell_confirmation() {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        let mut context = PermissionContext { tool_name: "Bash".into(), tool_input: serde_json::json!({"command":"git status"}), conversation_id: "one".into(), user_id: None, workspace_path: None };
        manager.set_session_rules("one", HashMap::from([("Bash(git *)".into(), PermissionAction::Allow), ("Bash(git push*)".into(), PermissionAction::Deny)]));
        manager.set_mode(PermissionMode::BypassPermissions);
        assert_eq!(manager.check_permission(&context), PermissionResult::Granted);
        context.tool_input = serde_json::json!({"command":"git push origin main"});
        assert!(matches!(manager.check_permission(&context), PermissionResult::Denied(_)));
        manager.set_session_rules("one", HashMap::from([("Bash(git *)".into(), PermissionAction::Allow)]));
        context.tool_input = serde_json::json!({"command":"git status; evil"});
        assert!(matches!(manager.check_permission(&context), PermissionResult::RequiresConfirmation(_)));
        manager.set_mode(PermissionMode::PlanMode);
        assert!(matches!(manager.check_permission(&context), PermissionResult::Denied(_)));
        context.tool_name = "Read".into();
        manager.set_session_rules("one", HashMap::from([("Read".into(), PermissionAction::Confirm)]));
        assert!(matches!(manager.check_permission(&context), PermissionResult::RequiresConfirmation(_)));
    }

    #[test]
    fn explicit_denies_survive_bypass_and_allow_cannot_override_plan() {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        let context = PermissionContext { tool_name: "Write".into(), tool_input: serde_json::json!({"path":"test"}), conversation_id: "one".into(), user_id: None, workspace_path: None };
        manager.set_session_rules("one", HashMap::from([("Write".into(), PermissionAction::Deny)]));
        manager.set_mode(PermissionMode::BypassPermissions);
        assert!(matches!(manager.check_permission(&context), PermissionResult::Denied(_)));
        let other = PermissionContext { conversation_id: "two".into(), ..context.clone() };
        assert_eq!(manager.check_permission(&other), PermissionResult::Granted);
        manager.set_session_rules("one", HashMap::from([("Write".into(), PermissionAction::Allow)]));
        manager.set_mode(PermissionMode::PlanMode);
        assert!(matches!(manager.check_permission(&context), PermissionResult::Denied(_)));
        manager.set_mode(PermissionMode::AskPermissions);
        assert_eq!(manager.check_permission(&context), PermissionResult::Granted);
    }
}
