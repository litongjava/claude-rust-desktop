//! Run-local agent definitions. Never written to the installed agent registry.
use super::provider_manager::ResolvedProvider;
use crate::services::agents::Definition;
use std::sync::Arc;

#[derive(Clone)]
pub struct RuntimeAgent {
    pub definition: Definition,
    pub provider: Option<ResolvedProvider>,
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
