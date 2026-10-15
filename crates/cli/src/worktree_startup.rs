//! CLI worktrees use the shared Desktop conversation binding and resolver.
use crate::{args::Cli, session::Session};
use anyhow::{Context, Result};
use claude_core::worktree::session as worktrees;

pub fn validate(cli: &Cli, source: &std::path::Path) -> Result<()> {
    if let Some(name) = &cli.worktree {
        worktrees::validate_named(source, (!name.is_empty()).then_some(name.as_str()))?;
    }
    Ok(())
}

pub fn apply(cli: &Cli, session: &mut Session) -> Result<()> {
    let Some(name) = &cli.worktree else {
        return Ok(());
    };
    let source = session.workspace.clone();
    let prepared = worktrees::prepare_named(&source, (!name.is_empty()).then_some(name.as_str()))?;
    let saved = session
        .store
        .db
        .with_conn(|conn| -> Result<()> {
            let transaction = conn.unchecked_transaction()?;
            let project: String = transaction.query_row(
                "SELECT project_id FROM conversations WHERE id=?1",
                [&session.id],
                |row| row.get(0),
            )?;
            prepared.save(
                &transaction,
                &session.id,
                &project,
                &source.to_string_lossy(),
            )?;
            transaction.execute(
                "UPDATE conversations SET workspace_path=?1 WHERE id=?2",
                [prepared.path.to_string_lossy().as_ref(), &session.id],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .and_then(|result| result);
    if let Err(error) = saved {
        prepared.cleanup();
        return Err(error);
    }
    session.workspace = session
        .store
        .workspace(&session.id, &session.paths.data_dir.join("workspaces"))?;
    std::env::set_current_dir(&session.workspace).context("Cannot enter managed worktree")?;
    eprintln!("Worktree: {}\nSession: {}. This worktree is retained on exit; use --resume {} to continue.",session.workspace.display(),session.id,session.id);
    Ok(())
}
