//! Definitions only: all execution remains in NativeEngine and its existing tool loop.
use super::ServiceContext;
use crate::db::DbManager;
use anyhow::{ensure, Context, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Definition {
    #[serde(alias = "name")]
    pub id: String,
    pub description: String,
    pub system_prompt: String,
    /// None uses the host's tools and permissions; a list is a strict additional limit.
    pub tools: Option<Vec<String>>,
}
pub fn canonical(id: &str) -> &str {
    match id {
        "claude-code-guide" => "guide",
        "verification" => "verify",
        "general-purpose" => "general",
        other => other,
    }
}
pub fn builtins() -> Vec<Definition> {
    [
        ("guide","Explain application and project workflows using available evidence.",true),
        ("explore","Explore the codebase, find relevant implementations, and report concrete file references.",true),
        ("plan","Investigate the task and produce an actionable implementation and verification plan without changing files.",true),
        ("verify","Verify the requested behavior with relevant tests and evidence. Report failures and limitations accurately.",false),
        ("general","Complete the requested task using the shared tools, respecting project instructions and host permissions.",false),
    ].into_iter().map(|(id,prompt,read_only)| Definition {
        id:id.into(), description:prompt.into(), system_prompt:prompt.into(),
        tools:read_only.then(|| ["Read","ListDir","Grep","WebFetch","WebSearch","memory_search","AskUserQuestion"].into_iter().map(String::from).collect()),
    }).collect()
}
impl Definition {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 64
                && self
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "Invalid agent ID"
        );
        ensure!(
            !self.system_prompt.trim().is_empty() && self.system_prompt.len() <= 32_000,
            "Invalid agent system prompt"
        );
        if let Some(tools) = &self.tools {
            ensure!(
                tools.len() <= 100 && tools.iter().all(|t| !t.trim().is_empty()),
                "Invalid agent tools"
            );
        }
        Ok(())
    }
}
pub fn list(context: &ServiceContext) -> Result<Vec<Definition>> {
    let mut definitions = builtins();
    definitions.extend(
        context
            .setting::<Vec<Definition>>("agent-definitions")?
            .unwrap_or_default(),
    );
    Ok(definitions)
}
pub fn resolve(context: &ServiceContext, id: &str) -> Result<Definition> {
    let id = id.to_lowercase();
    list(context)?
        .into_iter()
        .find(|d| d.id == canonical(&id))
        .context("Unknown agent type; use agents list")
}
pub fn install(context: &ServiceContext, definition: Definition) -> Result<()> {
    definition.validate()?;
    ensure!(
        canonical(&definition.id) == definition.id
            && !builtins().iter().any(|d| d.id == definition.id),
        "Cannot replace a built-in agent or alias"
    );
    context.db.with_conn(|c| -> Result<()> {
        let tx = c.unchecked_transaction()?;
        let raw: Option<String> = tx.query_row("SELECT value FROM peripheral_settings WHERE key='agent-definitions'",[],|r|r.get(0)).optional()?;
        let mut definitions: Vec<Definition> = raw.map(|v|serde_json::from_str(&v)).transpose()?.unwrap_or_default();
        definitions.retain(|d|d.id!=definition.id); definitions.push(definition);
        tx.execute("INSERT INTO peripheral_settings(key,value) VALUES('agent-definitions',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_string(&definitions)?])?;
        tx.commit()?; Ok(())
    })?
}
pub fn bind(db: &DbManager, session: &str, definition: &Definition) -> Result<()> {
    definition.validate()?;
    let payload = serde_json::to_string(definition)?;
    db.with_conn(|c|c.execute("INSERT INTO session_agents(conversation_id,definition) VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET definition=excluded.definition", rusqlite::params![session,payload]))??;
    Ok(())
}
pub fn for_session(db: &DbManager, session: &str) -> Result<Option<Definition>> {
    let value: Option<String> = db.with_conn(|c| {
        c.query_row(
            "SELECT definition FROM session_agents WHERE conversation_id=?1",
            [session],
            |r| r.get(0),
        )
        .optional()
    })??;
    value
        .map(|v| serde_json::from_str(&v).map_err(Into::into))
        .transpose()
}
