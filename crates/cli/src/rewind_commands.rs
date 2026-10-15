//! Explicit, previewable code/conversation rewind without destroying the source transcript.
use crate::session::Session;
use anyhow::{ensure, Context, Result};
use claude_core::{
    db::{conversation_repo, message_repo},
    runtime::RuntimePaths,
    services::checkpoints,
};

pub async fn handle(
    command: &str,
    argument: &str,
    paths: &RuntimePaths,
    session: &mut Session,
) -> Result<bool> {
    if command != "/rewind" {
        return Ok(false);
    }
    let mut args = argument.split_whitespace();
    let action = args.next().unwrap_or("list");
    if action == "list" {
        ensure!(
            args.next().is_none(),
            "Usage: /rewind [list|preview ID|code ID|conversation ID|both ID]"
        );
        let saved = checkpoints::list(&paths.data_dir, &session.id)?;
        if saved.is_empty() {
            eprintln!("No checkpoints captured for this session. Checkpoints are created around turns in supported Git worktrees.");
        }
        for checkpoint in saved {
            eprintln!(
                "{}  {}  {} file changes{}\n  message: {}",
                checkpoint.id,
                checkpoint.created_at,
                checkpoint.changes.len(),
                if checkpoint.restored {
                    " (code restored)"
                } else {
                    ""
                },
                checkpoint.message_id
            );
        }
        eprintln!("/rewind preview ID previews conflicts. Choose /rewind code ID, conversation ID, or both ID. Code restores only this turn's recorded file-edit tool targets. Bash and unrelated workspace changes are not tracked; later edits to the same files are protected.");
        return Ok(true);
    }
    ensure!(
        ["preview", "code", "conversation", "both"].contains(&action),
        "Choose preview, code, conversation, or both"
    );
    let id = args.next().context("Checkpoint ID required")?;
    ensure!(
        args.next().is_none(),
        "Unexpected argument after checkpoint ID"
    );
    let checkpoint = checkpoints::get(&paths.data_dir, &session.id, id)?;
    let workspace = session
        .store
        .workspace(&session.id, &paths.data_dir.join("workspaces"))?
        .canonicalize()?;
    ensure!(
        workspace == checkpoint.workspace,
        "Checkpoint belongs to a different working directory"
    );
    if action == "preview" {
        println!(
            "{}",
            serde_json::to_string_pretty(&checkpoints::preview(&paths.data_dir, &session.id, id)?)?
        );
        return Ok(true);
    }
    if action == "code" || action == "both" {
        let report = checkpoints::preview(&paths.data_dir, &session.id, id)?;
        ensure!(!report.restored, "Checkpoint code was already restored");
        ensure!(
            report.conflicts.is_empty(),
            "Files changed after the checkpoint: {}",
            report.conflicts.join(", ")
        );
    }
    let target = if action == "conversation" || action == "both" {
        let messages = session
            .store
            .db
            .with_conn(|conn| message_repo::get_messages_by_conversation(conn, &session.id))??;
        let index = messages
            .iter()
            .position(|message| message.id == checkpoint.message_id)
            .context("Checkpoint message is not present in this conversation")?;
        Some(if index == 0 {
            session.store.clear(&session.id)?
        } else {
            session.store.db.with_conn(|conn| {
                conversation_repo::branch_conversation(conn, &session.id, None, Some(index - 1))
            })??
        })
    } else {
        None
    };
    if action == "code" || action == "both" {
        let report = checkpoints::restore(&paths.data_dir, &session.id, id)?;
        eprintln!(
            "Restored {} file changes from checkpoint {id}.",
            report.paths.len()
        );
    }
    if let Some(target) = target {
        crate::background_commands::copy_settings(session, &target)?;
        let previous = session.id.clone();
        session.resume_saved(paths, &target).await?;
        eprintln!("Conversation rewound to before message {} in session {target}. Original transcript retained as {previous}.", checkpoint.message_id);
    }
    Ok(true)
}
