//! Run-local agent definitions. Never written to the installed agent registry.
use super::provider_manager::{ProviderManager, ResolvedProvider};
use crate::services::agents::Definition;
use std::sync::Arc;

/// Keep the selector across SDK turns. Credentials are resolved at invocation.
#[derive(Clone)]
pub struct AgentModel {
    pub provider_id: String,
    pub model: String,
}

impl AgentModel {
    pub fn resolve(&self, providers: &ProviderManager) -> anyhow::Result<ResolvedProvider> {
        if providers.binding.read().is_some() { return providers.binding.resolve(Some((&self.provider_id, &self.model))).ok_or_else(|| anyhow::anyhow!("SDK agent model is not configured")); }
        providers
            .resolve_selected(&self.provider_id, &self.model)
            .ok_or_else(|| anyhow::anyhow!("SDK agent model is not configured"))
    }
}

#[derive(Clone)]
pub struct RuntimeAgent {
    pub definition: Definition,
    pub provider: Option<AgentModel>,
    pub disallowed_tools: Vec<String>,
    pub max_turns: Option<u32>,
    /// Named MCP servers this agent may use; `mcpServers` string entries.
    pub mcp_servers: Vec<String>,
    /// Skill instructions preloaded into the child system prompt.
    pub skill_context: Option<String>,
    /// Agent memory scope: user, project or local.
    pub memory: Option<String>,
    /// Permission mode requested by the SDK definition.
    pub permission_mode: Option<String>,
    /// Reasoning effort for the agent's requests.
    pub effort: Option<String>,
    /// Only used when the SDK host selects this agent for the main thread.
    pub initial_prompt: Option<String>,
    /// Appended to the child system prompt.
    pub reminder: Option<String>,
}
pub type Catalog = Arc<Vec<RuntimeAgent>>;
