//! Production CLI entry points must encode origin before the positional tail.
use regex::Regex;
use std::path::Path;

fn bypasses(source: &str) -> Vec<&str> {
    static PATTERNS: std::sync::OnceLock<Vec<Regex>> = std::sync::OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| [
        // Positional prompts appended to argv or directly to a Command.
        r#"(?s)"--"(?:\.into\(\))?(?:\s*,\s*&?|\)\s*\.arg\()\s*(?:prompt|directive|instruction|query|input)\b"#,
        r#"\.arg\(\s*&?(?:prompt|directive|instruction|query)\b"#,
        // A direct self launch cannot add a positional argument, even a literal.
        r#"(?s)Command::new\(\s*(?:"dcl(?:\.exe)?"|(?:std::)?env::current_exe\(\)[?]?)\)\s*\.arg\(\s*"[^-"]"#,
        r#"Command::new\(\s*"dcl(?:\.exe)?"\s*\)"#,
        // A self-launch with literal/renamed text and no positional delimiter.
        r#"(?s)Command::new\(\s*(?:"dcl(?:\.exe)?"|(?:std::)?env::current_exe\(\))[^;]{0,1500}?\.(?:arg|args)\(\s*(?:\[\s*)?"(?:-p|--print|query)""#,
    ].into_iter().map(|p| Regex::new(p).unwrap()).collect());
    let mut hits = Vec::new();
    for pattern in patterns {
        hits.extend(pattern.find_iter(source).map(|hit| hit.as_str()));
    }
    hits
}

// A launcher can hide its positional argument behind arbitrary variable names.
// Require review of every production function that resolves the running binary,
// including helpers that return it for a separate spawn site.
fn reviewed_self_lookup(file: &str, function: &str) -> bool {
    matches!(
        (file, function),
        ("workflow_runner.rs" | "remote_host.rs" | "workflow_commands.rs", "run")
        | ("batch_commands.rs", "launch")
        | ("worker_frontend.rs", "single_turn")
        | ("restart.rs", "command")
        | ("worker_binding.rs", "retain_for_transfer" | "start_retirement")
        | ("background_commands.rs", "handle")
        | ("skill_commands.rs", "prepare_invocation")
        | ("child_session.rs", "standalone_child_executable")
        | ("script_workflow.rs", "workflow_child_command")
        | ("toolscript_job.rs", "start_bound_job")
        | ("request_identity.rs", "build")
        | ("checks.rs", "current")
        | ("pty_gate.rs", "gate_command")
        | ("pty_supervisor.rs", "supervisor_command")
        | ("conpty.rs", "start")
        | ("sandbox.rs", "bridge_executable")
        | ("dcd-server.rs", "default_web_dir")
        // Inline platform tests and their fixtures.
        | ("async_hooks.rs", "shared_delivery_is_once_and_requires_live_owner" | "process_exit_kills_background_tree")
        | ("workspace.rs", "spawn_git_fixture" | "git_fixture_command")
        | ("background_shell.rs", "cross_process_writer_feeds_an_owner_in_another_process" | "owner_loss_stops_a_cross_process_write_waiting_for_the_lock" | "spawn")
    )
}

#[test]
fn dcl_prompt_launches_require_the_origin_builder() {
    for mutation in [
        r#"Command::new(std::env::current_exe()?).args(["-p", text]).spawn()"#,
        r#"Command::new("dcl").arg("--print").arg("hello").spawn()"#,
        r#"Command::new(std::env::current_exe()?).arg("hello").spawn()"#,
        r#"cmd.arg("--").arg(instruction).spawn()"#,
        r#"args.extend(["--".into(), directive]);"#,
    ] {
        assert!(!bypasses(mutation).is_empty(), "missed {mutation}");
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let test_modules = Regex::new(r"(?ms)^#\[cfg\(test\)\]\s*mod \w+\s*\{.*?^\}").unwrap();
    let symbols = Regex::new(r"\bfn\s+(\w+)\s*[<(]|\b(current_exe)\s*\(").unwrap();
    let mut violations = Vec::new();
    for tree in ["crates/core/src", "crates/cli/src", "src-tauri/src"] {
        for entry in walkdir::WalkDir::new(root.join(tree)) {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "rs")
                || path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("tests.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(path).unwrap();
            let source = test_modules.replace_all(&source, "");
            let mut production = source.into_owned();
            if path.ends_with("services/background_sessions.rs") {
                let start = production.find("impl PromptOrigin {").unwrap();
                let end = start + production[start..].find("\n}").unwrap() + 2;
                production.replace_range(start..end, "");
            }
            let file = path.file_name().unwrap().to_str().unwrap();
            let mut function = "";
            for hit in symbols.captures_iter(&production) {
                if let Some(name) = hit.get(1) {
                    function = name.as_str();
                } else if !reviewed_self_lookup(file, function) {
                    violations.push(format!(
                        "Unreviewed executable lookup in {}::{function}",
                        path.display()
                    ));
                }
            }
            for hit in bypasses(&production) {
                violations.push(format!("{}: {hit}", path.display()));
            }
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
