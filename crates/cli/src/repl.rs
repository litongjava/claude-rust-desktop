//! Local REPL commands use the same stores as Desktop and never call a model.
use crate::{args::McpCommands, session::Session};
use anyhow::{ensure, Result};
use claude_core::{
    db::conversation_repo,
    runtime::{RuntimePaths, SessionStore},
    skills::SkillsManager,
};

pub fn tool_details(session: &Session) -> Result<Vec<(String, String)>> {
    session.store.db.with_conn(|conn| {
        let rows = claude_core::db::message_repo::get_messages_by_conversation(conn, &session.id)?;
        let Some(message) = rows.iter().rev().find(|row| row.role == "assistant") else {
            return Ok(Vec::new());
        };
        let tools = claude_core::db::message_repo::get_tool_calls_for_message(conn, &message.id)?;
        Ok(tools
            .into_iter()
            .map(|tool| {
                let title = format!(
                    "{} · {}",
                    tool.name,
                    if tool.is_error { "error" } else { "completed" }
                );
                let body = format!(
                    "Input:\n{}\n\nOutput:\n{}",
                    tool.input.as_deref().unwrap_or("{}"),
                    tool.output.as_deref().unwrap_or("")
                );
                let mut shown: String = body.chars().take(65536).collect();
                if shown.len() < body.len() {
                    shown
                        .push_str("\n[Display truncated; full result remains in session storage.]");
                }
                (title, shown)
            })
            .collect())
    })?
}

pub async fn handle(line: &str, paths: &RuntimePaths, session: &mut Session) -> Result<bool> {
    let line = line.trim();
    let (command, argument) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    match command {
        "/compact" => {
            let report = tokio::select! {
                report = claude_core::services::compaction::compact(
                    &session.engine, &session.store.db, &session.id,
                    &session.provider_id, &session.model, argument.trim(),
                ) => report?,
                _ = tokio::signal::ctrl_c() => anyhow::bail!("Compaction cancelled; original history retained"),
            };
            eprintln!(
                "Summarized {} messages. Original transcript remains in /history.",
                report.summarized_messages
            );
        }
        "/clear" => {
            ensure!(argument.trim().is_empty(), "Usage: /clear");
            let previous = session.id.clone();
            session.clear()?;
            eprintln!(
                "New session: {}. Previous history retained: {previous}",
                session.id
            );
        }
        "/context" => {
            ensure!(argument.trim().is_empty(), "Usage: /context");
            let rows = session.store.db.with_conn(|conn| {
                claude_core::db::message_repo::get_messages_by_conversation(conn, &session.id)
            })??;
            let start = rows
                .iter()
                .rposition(|row| row.is_compact_boundary)
                .unwrap_or(0);
            let bytes: usize = rows[start..].iter().map(|row| row.content.len()).sum();
            eprintln!("Saved messages: {}\nActive messages: {}\nActive message text: {bytes} UTF-8 bytes\nCompacted: {}\nCounts exclude system instructions and tool payloads; this is not a token estimate.", rows.len(), rows.len() - start, rows.iter().any(|row| row.is_compact_boundary));
        }
        "/permissions" => {
            if !argument.trim().is_empty() {
                session.set_permission_mode(argument.trim()).await?;
            }
            eprintln!("Permission mode: {}", session.permission_mode());
        }
        "/rename" => {
            let title = argument.trim();
            rename(&session.store, &session.id, title)?;
            eprintln!("Session renamed: {title}");
        }
        "/sessions" => {
            ensure!(argument.trim().is_empty(), "Usage: /sessions");
            let sessions = session
                .store
                .db
                .with_conn(conversation_repo::list_conversations)??;
            println!("{}", serde_json::to_string_pretty(&sessions)?);
        }
        "/skills" => {
            ensure!(argument.trim().is_empty(), "Usage: /skills");
            let manager = SkillsManager::with_path(paths.data_dir.join("skills"));
            println!(
                "{}",
                serde_json::to_string_pretty(&manager.load_skills().await?)?
            );
        }
        "/mcp" => {
            ensure!(argument.trim().is_empty(), "Usage: /mcp");
            crate::mcp(paths, &McpCommands::List).await?;
        }
        "/end" | "/cancel" => eprintln!("No unfinished prompt. Use /multiline to compose one."),
        _ => return crate::extended_commands::handle(command, argument, paths, session).await,
    }
    Ok(true)
}

fn rename(store: &SessionStore, id: &str, title: &str) -> Result<()> {
    ensure!(!title.is_empty(), "Usage: /rename TITLE");
    ensure!(
        title.chars().count() <= 200,
        "Session titles must be at most 200 characters"
    );
    store
        .db
        .with_conn(|conn| conversation_repo::update_conversation_title(conn, id, title))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_persists_without_losing_history_and_rejects_empty_titles() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(dir.path(), "model", "provider").unwrap();
        store.append(&id, "user", "Keep this message").unwrap();
        let before = store.history(&id).unwrap();
        rename(&store, &id, "新的会话").unwrap();
        assert!(rename(&store, &id, "").is_err());
        let reopened = SessionStore::new(paths.open_database().unwrap());
        assert_eq!(
            reopened.resume(&id).unwrap().title.as_deref(),
            Some("新的会话")
        );
        assert_eq!(reopened.history(&id).unwrap(), before);
    }
}
