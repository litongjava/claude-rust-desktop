use super::*;
use crate::{
    runtime::{RuntimePaths, SessionStore},
    services::tool_hooks::HookRunner,
};
use axum::{extract::State, routing::post, Json, Router};

fn hook(windows: &str, unix: &str) -> Value {
    json!({"type":"command","shell":if cfg!(windows) {"powershell"} else {"sh"},"command":if cfg!(windows) {windows} else {unix},"timeout":10})
}
fn load_hooks(root: &std::path::Path, settings: Value) -> HookRunner {
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    std::fs::write(root.join(".claude/settings.json"), settings.to_string()).unwrap();
    HookRunner::load(&root.join("data"), root).unwrap()
}
fn provider(endpoint: &str) -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":endpoint,"api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    }
}
fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_str(
        std::fs::read_to_string(path)
            .unwrap()
            .trim_start_matches('\u{feff}'),
    )
    .unwrap()
}

#[tokio::test]
async fn failure_hooks_receive_actual_errors_and_success_uses_post_tool_use() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let hooks = load_hooks(
        temp.path(),
        json!({"hooks":{
            "PostToolUseFailure":[{"matcher":"^Bash$","hooks":[hook(
                "[IO.File]::WriteAllText('failure.json',[Console]::In.ReadToEnd()); [Console]::Error.Write('observational failure hook'); exit 2",
                "cat > failure.json; printf 'observational failure hook' >&2; exit 2")]}],
            "PostToolUse":[{"matcher":"^Bash$","hooks":[hook("[IO.File]::WriteAllText('success.json',[Console]::In.ReadToEnd())", "cat > success.json")]}]
        }}),
    );
    let (tx, _rx) = mpsc::channel(32);
    let manager = crate::runtime::default_permissions();
    manager.set_mode(crate::permissions::PermissionMode::BypassPermissions);
    let mut executor = ToolLoopExecutor::new(
        provider("http://127.0.0.1:1"),
        vec![],
        None,
        128,
        tx,
        temp.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id("hook-fixture".into())
    .with_permission_manager(manager)
    .with_hooks(hooks);
    let (_, result, failed) = executor
        .execute_tool_call("Bash", &json!({"command":"exit 7"}), "failed-call")
        .await;
    assert!(failed, "{result}");
    let event = read_json(&temp.path().join("failure.json"));
    assert_eq!(event["hook_event_name"], "PostToolUseFailure");
    assert_eq!(event["tool_use_id"], "failed-call");
    assert!(event["error"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(event["is_interrupt"], false);
    assert!(!temp.path().join("success.json").exists());
    let (_, result, failed) = executor
        .execute_tool_call(
            "Bash",
            &json!({"command":"echo success"}),
            "successful-call",
        )
        .await;
    assert!(!failed, "{result}");
    let event = read_json(&temp.path().join("success.json"));
    assert_eq!(event["hook_event_name"], "PostToolUse");
    assert_eq!(event["tool_use_id"], "successful-call");
    assert!(event.get("error").is_none());
}

#[tokio::test]
async fn child_start_context_and_stop_continuation_are_isolated_from_parent_stop() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let sessions = SessionStore::new(paths.open_database().unwrap());
    let parent = sessions.create(temp.path(), "fixture", "fixture").unwrap();
    let hooks = load_hooks(
        temp.path(),
        json!({"hooks":{
            "SubagentStart":[{"matcher":"^general$","hooks":[hook(
                "[IO.File]::WriteAllText('start.json',[Console]::In.ReadToEnd()); Write-Output 'CHILD_START_CONTEXT'",
                "cat > start.json; printf CHILD_START_CONTEXT")]}],
            "SubagentStop":[{"matcher":"^general$","hooks":[hook(
                "[IO.File]::WriteAllText('stop.json',[Console]::In.ReadToEnd()); if (!(Test-Path continued)) { [IO.File]::WriteAllText('continued','yes'); Write-Output '{\"decision\":\"block\",\"reason\":\"one extra verification\"}' }",
                "cat > stop.json; if test ! -f continued; then printf yes > continued; printf '%s' '{\"decision\":\"block\",\"reason\":\"one extra verification\"}'; fi")]}],
            "Stop":[{"hooks":[hook("[IO.File]::WriteAllText('wrong-parent-stop','bad')", "printf bad > wrong-parent-stop")]}]
        }}),
    );
    async fn reply(
        State(requests): State<Arc<std::sync::Mutex<Vec<Value>>>>,
        Json(body): Json<Value>,
    ) -> impl axum::response::IntoResponse {
        requests.lock().unwrap().push(body);
        (
            [("content-type", "text/event-stream")],
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"content":"child complete"},"finish_reason":"stop"}]})
            ),
        )
    }
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, _rx) = mpsc::channel(128);
    let mut executor = ToolLoopExecutor::new(
        provider(&endpoint),
        vec![],
        None,
        128,
        tx,
        temp.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id(parent.clone())
    .with_search_config_path(paths.data_dir.join("config.json"))
    .with_permission_manager(crate::runtime::default_permissions())
    .with_hooks(hooks);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        executor.execute_delegated(&json!({"prompt":"child task","agent_type":"general"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["status"], "completed");
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].to_string().contains("CHILD_START_CONTEXT"));
    assert!(calls[1].to_string().contains("one extra verification"));
    let start = read_json(&temp.path().join("start.json"));
    let stop = read_json(&temp.path().join("stop.json"));
    assert_eq!(start["session_id"], parent);
    assert_eq!(start["agent_id"], result["session_id"]);
    assert_eq!(stop["agent_id"], result["session_id"]);
    assert_eq!(stop["stop_hook_active"], true);
    assert_eq!(stop["last_assistant_message"], "child complete");
    assert!(!temp.path().join("wrong-parent-stop").exists());
    server.abort();
}
