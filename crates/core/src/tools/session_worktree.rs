//! EnterWorktree and ExitWorktree, as in the reference CLI. They reuse the
//! session worktree binding behind `dcl --worktree` and `/worktree`, so the
//! prompt, tools and later turns all resolve the same working directory.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::db::DbManager;
use crate::worktree::{hooks, session as worktrees};

pub fn definitions() -> Vec<super::ToolDefinition> {
    vec![
        super::ToolDefinition {
            name: "EnterWorktree".into(),
            description: "Bind this session to a Git worktree. With `path`, it enters a worktree already listed by `git worktree list` for this repository; otherwise it creates one from HEAD at `<git-common-dir>/desktop-worktrees/<name>` on branch `worktree/<name>` (`session/<uuid>` when no name is given). A project subdirectory is preserved and the binding is recorded for the conversation; later prompts and tool calls run there. The session must not already be in a worktree, the project must be trusted, and `name` and `path` are mutually exclusive. ExitWorktree returns to the original directory.".into(),
            input_schema: json!({"type":"object","properties":{
                "name":{"description":"New worktree name, up to 64 characters in slash-separated segments of letters, digits, dots, underscores and dashes; a random UUID is used when omitted.","type":"string"},
                "path":{"description":"Existing worktree listed by `git worktree list` for this repository, but not the main checkout.","type":"string"}
            },"additionalProperties":false}),
        },
        super::ToolDefinition {
            name: "ExitWorktree".into(),
            description: "Leave the worktree bound to this session by EnterWorktree or `dcl --worktree` and return to the original checkout. action \"keep\" preserves the directory and branch; \"remove\" deletes the directory but keeps its branch, and is refused for a worktree you entered by path rather than created here. When removing a worktree with uncommitted or untracked files, the call lists them and stops unless discard_changes: true is passed; confirm with the user first. With no bound worktree, nothing changes.".into(),
            input_schema: json!({"type":"object","properties":{
                "action":{"description":"\"keep\" preserves the directory and branch; \"remove\" deletes the directory and keeps the branch.","type":"string","enum":["keep","remove"]},
                "discard_changes":{"description":"Pass true to allow removal despite uncommitted or untracked files; otherwise the call lists them and stops.","type":"boolean"}
            },"required":["action"],"additionalProperties":false}),
        },
    ]
}

pub fn is_worktree_tool(name: &str) -> bool {
    matches!(name, "EnterWorktree" | "ExitWorktree")
}

fn entered_key(session: &str) -> String {
    format!("entered-worktree.{session}")
}

/// Returns the tool result and the session's new working directory.
pub async fn execute(
    name: &str,
    input: &Value,
    root: &Path,
    session: &str,
    cwd: &Path,
) -> Result<(Value, PathBuf)> {
    let context = crate::services::ServiceContext::open(
        crate::runtime::RuntimePaths::new(root.to_path_buf())?,
        std::sync::Arc::new(crate::services::EnvironmentCredentials),
    )?;
    let db = &context.db;
    match name {
        "EnterWorktree" => {
            ensure!(
                hooks::status(db, session)?.is_none(),
                "This session is already in a worktree; use ExitWorktree first"
            );
            ensure!(
                input["name"].is_null() || input["path"].is_null(),
                "name and path are mutually exclusive"
            );
            let project = db
                .with_conn(|conn| {
                    crate::native_engine::workspace::conversation_project(conn, session)
                })??
                .context("Worktrees require a trusted project")?;
            let (path, created) = match input["path"].as_str() {
                Some(path) => (
                    bind_existing(db, session, &project.id, cwd, Path::new(path))?,
                    false,
                ),
                None => (
                    create(db, session, &project.id, cwd, input["name"].as_str())?,
                    true,
                ),
            };
            context.save_setting(&entered_key(session), &json!({"created":created}))?;
            Ok((
                json!({"worktree_path":path,"content":format!("Switched to worktree {}. The session now works there; use ExitWorktree to return.", path.display())}),
                path,
            ))
        }
        "ExitWorktree" => {
            let Some(state) = hooks::status(db, session)? else {
                return Ok((
                    json!({"content":"No worktree session is active; nothing was changed."}),
                    cwd.to_path_buf(),
                ));
            };
            let action = input["action"].as_str().context("action is required")?;
            let path = PathBuf::from(state["path"].as_str().unwrap_or_default());
            let created = context
                .setting::<Value>(&entered_key(session))?
                .map_or(true, |entered| entered["created"] != false);
            let source = match action {
                "keep" => hooks::release(root, db, session, false).await?,
                "remove" => {
                    ensure!(created, "This worktree was entered by path, so ExitWorktree will not remove it; use action \"keep\"");
                    let dirty =
                        worktrees::git(&path, &["status", "--porcelain", "--untracked-files=all"])?;
                    if !dirty.is_empty() && input["discard_changes"] != true {
                        bail!("The worktree has uncommitted files; confirm with the user, then pass discard_changes: true to discard them:\n{dirty}");
                    }
                    if input["discard_changes"] == true {
                        let source = hooks::release(root, db, session, false).await?;
                        let repo = worktrees::git(&source, &["rev-parse", "--show-toplevel"])?;
                        worktrees::git(
                            Path::new(&repo),
                            &["worktree", "remove", "--force", &path.to_string_lossy()],
                        )?;
                        source
                    } else {
                        hooks::release(root, db, session, true).await?
                    }
                }
                other => bail!("Unsupported action: {other}"),
            };
            context.save_setting(&entered_key(session), &Value::Null)?;
            Ok((
                json!({"working_directory":source,"content":format!("Left the worktree ({action}). Working directory: {}", source.display())}),
                source,
            ))
        }
        _ => bail!("Unknown worktree tool {name}"),
    }
}

fn create(
    db: &DbManager,
    session: &str,
    project: &str,
    source: &Path,
    name: Option<&str>,
) -> Result<PathBuf> {
    let prepared = worktrees::prepare_named(source, name.filter(|n| !n.is_empty()))?;
    let saved = db
        .with_conn(|conn| -> Result<()> {
            let transaction = crate::db::write_transaction(&conn)?;
            prepared.save(&transaction, session, project, &source.to_string_lossy())?;
            transaction.execute(
                "UPDATE conversations SET workspace_path=?1 WHERE id=?2",
                [prepared.path.to_string_lossy().as_ref(), session],
            )?;
            transaction.commit()?;
            Ok(())
        })
        .and_then(|result| result);
    if let Err(error) = saved {
        prepared.cleanup();
        return Err(error);
    }
    Ok(prepared.path.clone())
}

fn bind_existing(
    db: &DbManager,
    session: &str,
    project: &str,
    source: &Path,
    path: &Path,
) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("Worktree path does not exist: {}", path.display()))?;
    let repo = worktrees::git(source, &["rev-parse", "--show-toplevel"])?;
    let listed = worktrees::git(Path::new(&repo), &["worktree", "list", "--porcelain"])?;
    let registered = listed
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|line| Path::new(line).canonicalize().ok().as_deref() == Some(path.as_path()));
    ensure!(
        registered,
        "{} is not listed by `git worktree list` for this repository",
        path.display()
    );
    ensure!(
        path != Path::new(&repo).canonicalize()?,
        "That path is the main checkout, not a worktree"
    );
    let branch = worktrees::git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    db.with_conn(|conn| -> Result<()> {
        let transaction = crate::db::write_transaction(&conn)?;
        transaction.execute("INSERT INTO conversation_worktrees (conversation_id, project_id, source_path, repo_path, path, branch) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![session, project, source.to_string_lossy(), repo, path.to_string_lossy(), branch])?;
        transaction.execute("UPDATE conversations SET workspace_path=?1 WHERE id=?2", [path.to_string_lossy().as_ref(), session])?;
        transaction.commit()?;
        Ok(())
    })??;
    Ok(path)
}
