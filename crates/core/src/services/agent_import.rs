//! Explicit conversion of the historical AgentsService JSON schema.
use super::{
    agents::{self, Definition},
    ServiceContext,
};
use crate::db::session_store::AgentCatalog;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyDefinition {
    pub agent_type: String,
    pub name: String,
    pub description: String,
    pub when_to_use: String,
    pub tools: Vec<String>,
    pub model: String,
    pub system_prompt: String,
    pub source: String,
    pub base_dir: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub definition: Definition,
    pub preview: bool,
    pub imported: bool,
    pub skipped: bool,
    pub backup: Option<String>,
    pub notes: Vec<String>,
}

pub fn import(
    context: &ServiceContext<impl AgentCatalog>,
    source: &Path,
    id: &str,
    preview: bool,
) -> Result<Report> {
    ensure!(
        std::fs::metadata(source)?.len() <= 1024 * 1024,
        "Agent definition exceeds 1 MiB"
    );
    let legacy: LegacyDefinition = serde_json::from_slice(&std::fs::read(source)?)
        .context("Expected an old AgentDefinition JSON object")?;
    ensure!(
        matches!(
            legacy.agent_type.as_str(),
            "ClaudeCodeGuide" | "Explore" | "GeneralPurpose" | "Plan" | "Verification" | "Custom"
        ),
        "Unknown legacy agent_type"
    );
    ensure!(legacy.tools.len() <= 100, "Too many legacy tools");
    let mut tools = Vec::new();
    for tool in &legacy.tools {
        let mapped = match tool.as_str() {
            "file_read" => "Read",
            "file_write" => "Write",
            "file_edit" => "Edit",
            "search" => "Grep",
            "list_files" => "ListDir",
            "execute_command" => "Bash",
            _ => anyhow::bail!("Unsupported legacy tool: {tool}; define an explicit current-schema tool allowlist instead"),
        }.to_string();
        if !tools.contains(&mapped) {
            tools.push(mapped);
        }
    }
    let definition = Definition {
        id: id.to_string(),
        description: legacy.description.clone(),
        system_prompt: legacy.system_prompt.clone(),
        tools: Some(tools),
        read_only_shell: false,
        legacy: Some(legacy),
    };
    agents::validate_custom(&definition)?;
    let backup = if preview {
        None
    } else {
        Some(
            context
                .db
                .backup_definitions(&context.paths.data_dir)?
                .to_string_lossy()
                .into_owned(),
        )
    };
    let skipped = !context.db.save_definition(&definition, false, preview)?;
    Ok(Report { definition, preview, imported: !preview && !skipped, skipped, backup,
            notes: vec!["Legacy model/source/base_dir are preserved as provenance. Execution uses the explicitly selected host model and trusted project, as the old executor did not apply those fields.".into(), "Mapped tools are a strict allowlist, including an empty list; import does not execute an agent.".into()] })
}
