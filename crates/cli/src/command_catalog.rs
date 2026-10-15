//! Discoverable commands shared by terminal hints, completion, and help.
pub const COMMANDS: &[(&str, &str)] = &[
    (
        "/branch",
        "Branch this conversation and switch to it: [TITLE]",
    ),
    ("/config", "Show settings and commands to change them"),
    ("/copy", "Copy the latest assistant response: [N]"),
    ("/export", "Export transcript: FILE or clipboard"),
    ("/list-agents", "List available agent definitions"),
    (
        "/tasks",
        "Show saved project tasks and this session's child task states",
    ),
    ("/plan", "Enable read-only plan permissions"),
    ("/reload-skills", "Reload installed skills from disk"),
    ("/resume", "List saved conversations or resume one: [ID]"),
    (
        "/terminal-setup",
        "Show terminal capabilities and keyboard shortcuts",
    ),
    ("/help", "Show commands"),
    ("/exit", "Exit (also /quit)"),
    ("/quit", "Exit"),
    ("/status", "Session, project, provider and model"),
    ("/history", "Persisted conversation messages"),
    ("/rename", "Rename this session: /rename TITLE"),
    ("/sessions", "List saved sessions; resume with --resume ID"),
    ("/tools", "Available native tools"),
    ("/skills", "Installed skills"),
    ("/mcp", "Configured MCP servers"),
    ("/provider", "Inspect/configure providers: [ID|add|edit ID]"),
    ("/model", "Inspect/switch models: [ID]"),
    ("/test", "Test provider with an isolated request"),
    (
        "/compact",
        "Compress context into a summary: [instructions]",
    ),
    ("/clear", "Start a new session; keep previous history"),
    (
        "/context",
        "Inspect active history size and compaction boundary",
    ),
    (
        "/permissions",
        "Inspect/change mode: ask|accept-edits|plan|bypass",
    ),
    ("/multiline", "Compose multiple lines; /end sends"),
    ("/end", "Send a multiline prompt"),
    ("/cancel", "Discard an unfinished prompt"),
];

pub fn help() -> String {
    let mut text = String::from("Commands (type / to discover, Tab to complete):\n");
    for (name, description) in COMMANDS {
        text.push_str(&format!("  {name:<14} {description}\n"));
    }
    text.push_str("End a line with a single backslash to continue onto the next line.\nArrow keys edit/navigate session-only history; Ctrl+R searches history.\nCtrl+C discards unfinished composition; at the main prompt it exits.\nDuring a task, Ctrl+C cancels the task. Multiline content is sent literally.");
    text
}
