//! SDK agent descriptions are validated as one run-local transaction.
use crate::session::Session;
use anyhow::{ensure, Context, Result};
use claude_core::{
    native_engine::runtime_agents::{Catalog, RuntimeAgent},
    services::agents,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Agent {
    description: String,
    prompt: String,
    tools: Option<Vec<String>>,
    #[serde(default)]
    disallowed_tools: Vec<String>,
    model: Option<String>,
    max_turns: Option<u32>,
    #[serde(default)]
    mcp_servers: Vec<Value>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    background: bool,
    memory: Option<String>,
    permission_mode: Option<String>,
    effort: Option<Value>,
    initial_prompt: Option<String>,
    #[serde(rename = "criticalSystemReminder_EXPERIMENTAL")]
    reminder: Option<String>,
}

pub async fn prepare(session: &Session, value: Option<&Value>) -> Result<Catalog> {
    let definitions: BTreeMap<String, Agent> = match value {
        None => return Ok(Default::default()),
        Some(value) => {
            serde_json::from_value(value.clone()).context("Invalid initialize.agents")?
        }
    };
    ensure!(definitions.len() <= 100, "Too many SDK agent definitions");
    let mut catalog = Vec::new();
    for (id, agent) in definitions {
        ensure!(agent.mcp_servers.is_empty() && agent.skills.is_empty() && !agent.background
            && agent.memory.is_none() && agent.permission_mode.is_none() && agent.effort.is_none()
            && agent.initial_prompt.is_none() && agent.reminder.is_none(),
            "SDK agent {id}: MCP, skills, background, memory, permissionMode, effort and main-thread prompt options are not supported");
        ensure!(
            !agent.description.trim().is_empty() && agent.description.len() <= 32_000,
            "Invalid SDK agent description"
        );
        ensure!(
            agent.disallowed_tools.len() <= 100
                && agent
                    .disallowed_tools
                    .iter()
                    .all(|name| !name.trim().is_empty()),
            "Invalid SDK agent disallowedTools"
        );
        ensure!(
            agent.max_turns.is_none_or(|turns| turns > 0),
            "SDK agent maxTurns must be positive"
        );
        let definition = agents::Definition {
            id,
            description: agent.description,
            system_prompt: agent.prompt,
            tools: agent.tools,
            legacy: None,
        };
        agents::validate_custom(&definition)?;
        let provider = match agent.model.as_deref() {
            None | Some("inherit") => None,
            Some(model) => Some(session.sdk_resolve_model(model).await?),
        };
        catalog.push(RuntimeAgent {
            definition,
            provider,
            disallowed_tools: agent.disallowed_tools,
            max_turns: agent.max_turns,
        });
    }
    Ok(std::sync::Arc::new(catalog))
}
