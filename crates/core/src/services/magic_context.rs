//! Explicit project consent for feeding recent conversation text into Magic Docs.
use super::ServiceContext;
use crate::db::session_store::magic::MagicHistory;
use anyhow::Result;
use std::path::Path;

pub fn enabled(context: &ServiceContext<impl MagicHistory>, project_id: &str) -> Result<bool> {
    context.db.conversation_consent(project_id)
}
pub fn configure(
    context: &ServiceContext<impl MagicHistory>,
    root: &Path,
    project_id: &str,
    value: bool,
) -> Result<()> {
    context
        .db
        .configure_conversations(project_id, &std::fs::canonicalize(root)?, value)
}

pub fn collect(
    context: &ServiceContext<impl MagicHistory>,
    root: &Path,
    project_id: Option<&str>,
    memories: Vec<String>,
) -> Result<Option<String>> {
    collect_cancellable(context, root, project_id, memories, &|| false)
}

pub fn collect_cancellable(
    context: &ServiceContext<impl MagicHistory>,
    root: &Path,
    project_id: Option<&str>,
    memories: Vec<String>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<String>> {
    let root = std::fs::canonicalize(root)?;
    let projects = context.db.projects()?;
    let mut messages = Vec::new();
    let mut remaining = 12000usize;
    for project in projects {
        anyhow::ensure!(!cancelled(), "Conversation search cancelled");
        if project_id.is_some_and(|id| id != project.id)
            || project.is_archived
            || project.folders.is_empty()
            || project.folders.iter().any(|f| !f.trusted)
            || project
                .workspace_path
                .as_deref()
                .and_then(|p| std::fs::canonicalize(p).ok())
                .as_ref()
                != Some(&root)
            || !enabled(context, &project.id)?
        {
            continue;
        }
        let rows = context
            .db
            .conversation_excerpts(&project.id, &root, cancelled)?;
        for mut message in rows {
            let mut end = message.content.len().min(remaining);
            while !message.content.is_char_boundary(end) {
                end -= 1;
            }
            message.content.truncate(end);
            remaining -= end;
            if !message.content.is_empty() {
                messages.push(message);
            }
            if remaining == 0 {
                break;
            }
        }
        if remaining == 0 {
            break;
        }
    }
    if memories.is_empty() && messages.is_empty() {
        return Ok(None);
    }
    let mut memories = memories;
    let mut budget = 12000usize;
    for memory in &mut memories {
        let mut end = memory.len().min(budget);
        while !memory.is_char_boundary(end) {
            end -= 1;
        }
        memory.truncate(end);
        budget -= end;
    }
    loop {
        let input = serde_json::to_string(
            &serde_json::json!({"memories":memories,"project_conversations":messages}),
        )?;
        if input.len() <= 32000 {
            return Ok(Some(input));
        }
        if !messages.is_empty() {
            messages.pop();
        } else if !memories.is_empty() {
            memories.pop();
        } else {
            return Ok(None);
        }
    }
}
