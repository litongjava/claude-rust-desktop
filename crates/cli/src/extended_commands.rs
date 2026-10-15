//! Local commands backed by the shared conversation store and OS clipboard.
use crate::session::Session;
use anyhow::{ensure, Context, Result};
use claude_core::{db::conversation_repo, runtime::RuntimePaths};
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

pub async fn handle(
    command: &str,
    argument: &str,
    paths: &RuntimePaths,
    session: &mut Session,
) -> Result<bool> {
    let argument = argument.trim();
    match command {
        "/copy" => {
            let number = if argument.is_empty() {
                1
            } else {
                argument
                    .parse::<usize>()
                    .context("Usage: /copy [positive response number]")?
            };
            let history = session.store.history(&session.id)?;
            let text = response(&history, number)?;
            copy_text(text)?;
            eprintln!("Copied {} chars to clipboard.", text.chars().count());
        }
        "/export" => {
            ensure!(
                !argument.is_empty(),
                "Usage: /export FILE or /export clipboard"
            );
            let history = session.store.history(&session.id)?;
            let text = transcript(&session.id, &history);
            if argument == "clipboard" {
                copy_text(&text)?;
                eprintln!("Copied {} chars to clipboard.", text.chars().count());
            } else {
                let path = Path::new(argument.trim_matches('"'));
                export_file(path, &text)?;
                eprintln!("Exported conversation to {}", path.display());
            }
        }
        "/branch" => {
            let id = session.branch_saved(paths, argument).await?;
            eprintln!("Switched to branch {id}; original conversation retained.");
        }
        "/resume" => {
            if argument.is_empty() {
                let sessions = session
                    .store
                    .db
                    .with_conn(conversation_repo::list_conversations)??;
                println!("{}", serde_json::to_string_pretty(&sessions)?);
                eprintln!("Use /resume ID to switch conversations.");
            } else {
                session.resume_saved(paths, argument).await?;
                eprintln!(
                    "Resumed {} using saved provider {} and model {}.",
                    session.id, session.provider_id, session.model
                );
            }
        }
        "/plan" => {
            ensure!(
                argument.is_empty(),
                "Usage: /plan; use /permissions ask to leave plan mode"
            );
            session.set_permission_mode("plan").await?;
            eprintln!("Plan mode enabled. Use /permissions ask to leave plan mode.");
        }
        "/config" => {
            ensure!(argument.is_empty(), "Usage: /config");
            eprintln!("Data directory: {}\nProvider: {}\nModel: {}\nPermission mode: {}\nChange settings using /provider, /model, and /permissions.", paths.data_dir.display(), session.provider_id, session.model, session.permission_mode());
        }
        "/terminal-setup" => {
            ensure!(argument.is_empty(), "Usage: /terminal-setup");
            use std::io::IsTerminal;
            eprintln!("Interactive input: {}\nInteractive output: {}\nTab: complete commands; arrow keys: edit/history.\nMultiline: /multiline followed by /end, or a trailing backslash.\nClipboard: /copy [N]. Mouse selection and automatic copy are controlled by your terminal.", std::io::stdin().is_terminal(), std::io::stdout().is_terminal());
        }
        "/tasks" => {
            ensure!(argument.is_empty(), "Usage: /tasks");
            let workspace = session
                .store
                .workspace(&session.id, &paths.data_dir.join("workspaces"))?;
            let tasks = claude_core::tools::execute_tool(
                "TaskList",
                serde_json::json!({}),
                &workspace.to_string_lossy(),
            )?;
            eprintln!("Durable project task records (not a list of running processes):");
            println!("{}", serde_json::to_string_pretty(&tasks)?);
            let children = delegated_states(&session.store, &session.id)?;
            eprintln!("Recorded child sessions for {} (delegation waits for completion; saved states may be stale after a crash):", session.id);
            println!("{}", serde_json::to_string_pretty(&children)?);
        }
        "/list-agents" => {
            ensure!(argument.is_empty(), "Usage: /list-agents");
            let context = crate::services::context(paths)?;
            eprintln!("Available agent definitions (not running tasks):");
            for agent in claude_core::services::agents::list(&context)? {
                println!("{}: {}", agent.id, agent.description);
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn delegated_states(
    store: &claude_core::runtime::SessionStore,
    parent: &str,
) -> Result<Vec<Value>> {
    store.db.with_conn(|conn| -> Result<Vec<Value>> {
        let mut statement = conn.prepare(
            "SELECT value FROM peripheral_settings WHERE key LIKE 'delegation.%' ORDER BY key",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        let mut children = Vec::new();
        for row in rows {
            let value: Value =
                serde_json::from_str(&row?).context("Reading saved delegation state")?;
            if value["parent_session_id"].as_str() == Some(parent) {
                children.push(value);
            }
        }
        Ok(children)
    })?
}

fn response(history: &[Value], number: usize) -> Result<&str> {
    ensure!(number > 0, "Response number must be positive");
    history
        .iter()
        .rev()
        .filter(|row| row["role"] == "assistant")
        .filter_map(|row| row["content"].as_str())
        .filter(|text| !text.is_empty())
        .nth(number - 1)
        .context("No matching assistant response in this conversation")
}

fn transcript(id: &str, history: &[Value]) -> String {
    let mut text = format!("# Conversation {id}\n\n");
    for row in history {
        text.push_str(&format!(
            "## {}\n\n{}\n\n",
            row["role"].as_str().unwrap_or("unknown"),
            row["content"].as_str().unwrap_or("")
        ));
    }
    text
}

fn export_file(path: &Path, text: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "Cannot create {}; existing files are never overwritten",
                path.display()
            )
        })?;
    file.write_all(text.as_bytes())
        .context("Writing transcript")?;
    file.flush()?;
    Ok(())
}

fn copy_text(text: &str) -> Result<()> {
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("powershell.exe");
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference = 'Stop'; [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false); Set-Clipboard -Value ([Console]::In.ReadToEnd())"]);
        command.creation_flags(0x08000000);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = Command::new("pbcopy");
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let mut command = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        Command::new("wl-copy")
    } else {
        let mut command = Command::new("xclip");
        command.args(["-selection", "clipboard"]);
        command
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Cannot start OS clipboard helper")?;
    let written = child
        .stdin
        .take()
        .context("Clipboard stdin unavailable")?
        .write_all(text.as_bytes());
    let output = child
        .wait_with_output()
        .context("Waiting for clipboard helper")?;
    written.context("Sending text to clipboard helper")?;
    ensure!(
        output.status.success(),
        "Clipboard helper failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn task_view_scopes_children_to_parent_and_preserves_recorded_states() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let context = crate::services::context(&paths).unwrap();
        let store = claude_core::runtime::SessionStore::new(context.db.clone());
        context.save_setting("delegation.child-a", &json!({"parent_session_id":"parent-a","session_id":"child-a","status":"completed"})).unwrap();
        context
            .save_setting(
                "delegation.child-b",
                &json!({"parent_session_id":"parent-b","session_id":"child-b","status":"running"}),
            )
            .unwrap();
        context
            .save_setting("other-setting", &json!({"parent_session_id":"parent-a"}))
            .unwrap();
        let children = delegated_states(&store, "parent-a").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0]["session_id"], "child-a");
        assert_eq!(children[0]["status"], "completed");
        assert!(delegated_states(&store, "missing-parent")
            .unwrap()
            .is_empty());
        assert_eq!(
            context
                .setting::<Value>("delegation.child-b")
                .unwrap()
                .unwrap()["status"],
            "running"
        );
    }

    #[test]
    fn copy_uses_nth_nonempty_assistant_response() {
        let history = vec![
            json!({"role":"assistant","content":"第一条"}),
            json!({"role":"user","content":"question"}),
            json!({"role":"assistant","content":""}),
            json!({"role":"assistant","content":"last"}),
        ];
        assert_eq!(response(&history, 1).unwrap(), "last");
        assert_eq!(response(&history, 2).unwrap(), "第一条");
        assert!(response(&history, 0).is_err());
        assert!(response(&history, 3).is_err());
    }

    #[test]
    fn export_preserves_utf8_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conversation.md");
        let text = transcript("test", &[json!({"role":"assistant","content":"中文 🚀"})]);
        export_file(&path, &text).unwrap();
        assert!(export_file(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }
}
