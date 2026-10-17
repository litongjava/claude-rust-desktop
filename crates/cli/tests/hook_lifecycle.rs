//! Host-expanded skill shells use the same failure hook contract as native tools.
use serde_json::{json, Value};

#[tokio::test]
async fn failed_skill_shell_calls_failure_hook_without_starting_model() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let skill = workspace.join(".claude/skills/failing");
    std::fs::create_dir_all(&skill).unwrap();
    let shell = if cfg!(windows) { "powershell" } else { "sh" };
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nshell: {shell}\n---\nContext !`exit 7`\n"),
    )
    .unwrap();
    let failure = if cfg!(windows) {
        "[IO.File]::WriteAllText('failure.json',[Console]::In.ReadToEnd())"
    } else {
        "cat > failure.json"
    };
    let success = if cfg!(windows) {
        "[IO.File]::WriteAllText('wrong-success','bad')"
    } else {
        "printf bad > wrong-success"
    };
    let settings = json!({"hooks":{
        "PostToolUseFailure":[{"matcher":"Bash","hooks":[{"type":"command","shell":shell,"command":failure}]}],
        "PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","shell":shell,"command":success}]}]
    }});
    std::fs::write(
        workspace.join(".claude/settings.json"),
        settings.to_string(),
    )
    .unwrap();
    // A bound but deliberately unserved endpoint proves failure precedes a model request.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ccl"));
    command
        .current_dir(&workspace)
        .env("CLAUDE_CONFIG_DIR", temp.path().join("user-config"))
        .env("HOOK_FIXTURE_KEY", "placeholder")
        .args([
            "--data-dir",
            temp.path().join("data").to_str().unwrap(),
            "--base-url",
            &endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "HOOK_FIXTURE_KEY",
            "--model",
            "fixture",
            "--dangerously-skip-permissions",
            "--print",
            "--",
            "/failing",
        ])
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    let event: Value = serde_json::from_str(
        std::fs::read_to_string(workspace.join("failure.json"))
            .unwrap()
            .trim_start_matches('\u{feff}'),
    )
    .unwrap();
    assert_eq!(event["hook_event_name"], "PostToolUseFailure");
    assert!(event["error"].as_str().is_some_and(|s| s.contains('7')));
    assert!(!workspace.join("wrong-success").exists());
}
