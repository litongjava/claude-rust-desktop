//! Terminal adapters for persistent feature lessons.
use anyhow::{Context, Result};
use deepseek_code_core::runtime::RuntimePaths;
use std::collections::BTreeSet;

pub(crate) const LESSONS: &[(&str, &str, &str)] = &[
    ("context", "Explore a codebase", "Ask a question about a file by including its relative path. Use /context to inspect active history and /compact to summarize a long conversation."),
    ("modes", "Choose permissions", "Use /plan for read-only planning, /permissions ask for approval before tools, or /permissions accept-edits for routine edits. Explicit deny rules remain in force in every mode."),
    ("rewind", "Restore a checkpoint", "After an editing turn, run /rewind list, then /rewind preview ID. /rewind code ID restores recorded file edits; /rewind conversation ID branches the conversation."),
    ("background", "Run a background task", "Use /subtask PROMPT to start a worker with the current context. /tasks lists its state; /tasks show ID reads its result; /tasks cancel ID stops it."),
    ("memory", "Keep project instructions", "Use /memory edit to write CLAUDE.md or /memory local for personal project instructions. They are loaded on each turn."),
    ("mcp", "Connect external tools", "Use /mcp to inspect configured tool servers. The dcl mcp add command registers a stdio server; plugin-provided servers follow the plugin enabled state."),
    ("skills", "Create a reusable skill", "Create .claude/skills/NAME/SKILL.md with YAML name and description, followed by instructions. /reload-skills refreshes the menu; /NAME invokes the skill."),
    ("sessions", "Continue a conversation", "Use /rename TITLE to label this session, /resume to browse saved sessions, and /branch TITLE to continue a copy."),
    ("details", "Inspect work in progress", "Press Ctrl+O to expand tool details; PgUp/PgDn scrolls and Ctrl+O closes. Esc interrupts the active turn."),
    ("language", "Navigate code with LSP", "Install a language-service plugin, then inspect /ide status. /ide lsp FILE documentSymbol lists symbols. Position-based operations use 1-based line and character values."),
];

pub async fn handle(command: &str, argument: &str, paths: &RuntimePaths) -> Result<bool> {
    let argument = argument.trim();
    match command {
        "/powerup" => {
            let context = crate::services::context(paths)?;
            let mut completed = context
                .setting::<BTreeSet<String>>("cli.powerup.completed")?
                .unwrap_or_default();
            let words = argument.split_whitespace().collect::<Vec<_>>();
            match words.as_slice() {
                [] | ["list"] => {
                    command_eprintln!(
                        "Discover CLI features · {}/{} completed",
                        completed.len(),
                        LESSONS.len()
                    );
                    for (id, title, _) in LESSONS {
                        command_eprintln!(
                            "  {} {id:<12} {title}",
                            if completed.contains(*id) {
                                "✓"
                            } else {
                                "○"
                            }
                        );
                    }
                    command_eprintln!("/powerup TOPIC opens a lesson; /powerup done TOPIC records completion; /powerup reset clears progress.");
                }
                ["reset"] => {
                    completed.clear();
                    context.save_setting("cli.powerup.completed", &completed)?;
                    command_eprintln!("Lesson progress reset.");
                }
                [id] | ["read", id] | ["done", id] => {
                    let (_, title, text) = LESSONS
                        .iter()
                        .find(|(key, _, _)| key == id)
                        .context("Unknown lesson; use /powerup to list topics")?;
                    if words.first() == Some(&"done") {
                        completed.insert((*id).into());
                        context.save_setting("cli.powerup.completed", &completed)?;
                        command_eprintln!("Completed: {title}");
                    } else {
                        command_eprintln!("{title}\n\n{text}\n\nWhen finished: /powerup done {id}");
                    }
                }
                _ => anyhow::bail!("Usage: /powerup [TOPIC|read TOPIC|done TOPIC|reset]"),
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}
