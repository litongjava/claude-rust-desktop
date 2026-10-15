use super::*;

fn executor(root: &std::path::Path) -> ToolLoopExecutor {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let (tx, _rx) = mpsc::channel(32);
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"hook-test","name":"hook-test","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"test","name":"test","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    ToolLoopExecutor::new(
        provider,
        vec![],
        None,
        128,
        tx,
        root.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_hooks(crate::services::tool_hooks::HookRunner::load(root, root).unwrap())
}

fn write_hook(root: &std::path::Path, event: &str, command: &str) {
    std::fs::write(root.join("settings.json"), json!({"hooks":{event:[{"matcher":"Write","hooks":[{"type":"command","command":command,"shell":if cfg!(windows){"powershell"}else{"sh"}}]}]}}).to_string()).unwrap();
}

#[tokio::test]
async fn pre_tool_hook_blocks_real_write_before_execution() {
    let root = tempfile::tempdir().unwrap();
    write_hook(root.path(), "PreToolUse", "exit 2");
    let mut executor = executor(root.path());
    let (_, output, failed) = executor
        .execute_tool_call(
            "Write",
            &json!({"file_path":"blocked.txt","content":"must not exist"}),
            "test",
        )
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("hook"), "{output}");
    assert!(!root.path().join("blocked.txt").exists());
}

#[tokio::test]
async fn modified_input_is_executed_and_post_context_reaches_tool_result() {
    let root = tempfile::tempdir().unwrap();
    let hook_output = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"file_path":"actual.txt","content":"hook content"}}}"#;
    let command = if cfg!(windows) {
        format!("[Console]::Out.Write('{hook_output}')")
    } else {
        format!("printf '%s' '{hook_output}'")
    };
    write_hook(root.path(), "PreToolUse", &command);
    let path = root.path().join("settings.json");
    let mut config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let post_output = r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"Run formatter before completion"}}"#;
    let post_command = if cfg!(windows) {
        format!("[Console]::Out.Write('{post_output}')")
    } else {
        format!("printf '%s' '{post_output}'")
    };
    config["hooks"]["PostToolUse"] = json!([{"matcher":"Write","hooks":[{"type":"command","command":post_command,"shell":if cfg!(windows){"powershell"}else{"sh"}}]}]);
    std::fs::write(path, config.to_string()).unwrap();
    let mut executor = executor(root.path());
    let (actual, output, failed) = executor
        .execute_tool_call(
            "Write",
            &json!({"file_path":"original.txt","content":"original"}),
            "test",
        )
        .await;
    assert!(!failed, "{output}");
    assert!(
        output.contains("Run formatter before completion"),
        "{output}"
    );
    assert_eq!(actual["file_path"], "actual.txt");
    assert!(!root.path().join("original.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("actual.txt")).unwrap(),
        "hook content"
    );
}

#[tokio::test]
async fn stop_hook_can_continue_only_once() {
    let root = tempfile::tempdir().unwrap();
    write_hook(root.path(), "Stop", "exit 2");
    let mut executor = executor(root.path());
    assert!(executor.stop_hook("done").await.unwrap().is_some());
    assert!(executor
        .stop_hook("still done")
        .await
        .unwrap_err()
        .to_string()
        .contains("blocked again"));
}
