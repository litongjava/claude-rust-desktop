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
}
pub type Catalog = Arc<Vec<RuntimeAgent>>;
