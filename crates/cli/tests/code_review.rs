//! Real CLI command preparation, local model transport, and unchanged Git state.
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
async fn respond(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    requests.lock().unwrap().push(body);
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"Review complete; no edits."},"finish_reason":"stop"}]})
        ),
    )
}
#[tokio::test]
async fn explicit_review_injects_snapshot_preserves_worktree_and_honors_custom_override() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    git(&workspace, &["init", "-q"]);
    git(&workspace, &["config", "user.name", "Fixture"]);
    git(
        &workspace,
        &["config", "user.email", "fixture@example.invalid"],
    );
    let file = workspace.join("file with spaces.txt");
    std::fs::write(&file, "original\n").unwrap();
    git(&workspace, &["add", "--", "file with spaces.txt"]);
    git(&workspace, &["commit", "-qm", "initial"]);
    std::fs::write(&file, "snapshot-marker-unique\n").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let head = git(&workspace, &["rev-parse", "HEAD"]);
    for (index, prompt) in [
        r#"/code-review high path "file with spaces.txt""#,
        "/code-review path missing.txt",
        "/code-review custom instructions",
    ]
    .into_iter()
    .enumerate()
    {
        if index == 2 {
            let dir = workspace.join(".claude/commands");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("code-review.md"),
                "CUSTOM_REVIEW_OVERRIDE $ARGUMENTS",
            )
            .unwrap();
        }
        let status = git(&workspace, &["status", "--porcelain"]);
        let count = requests.lock().unwrap().len();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        command
            .current_dir(&workspace)
            .env("CLAUDE_CONFIG_DIR", temp.path().join("user-config"))
            .env("REVIEW_FIXTURE_KEY", "fixture-not-secret")
            .args([
                "--data-dir",
                temp.path().join(format!("data-{index}")).to_str().unwrap(),
                "--base-url",
                &endpoint,
                "--api-format",
                "openai",
                "--api-key-env",
                "REVIEW_FIXTURE_KEY",
                "--model",
                "fixture",
                "--print",
                "--output-format",
                "json",
                "--",
                prompt,
            ])
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let output = tokio::time::timeout(Duration::from_secs(30), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&workspace, &["status", "--porcelain"]), status);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "snapshot-marker-unique\n"
        );
        let captured = requests.lock().unwrap();
        if index == 1 {
            assert!(!output.status.success());
            assert_eq!(captured.len(), count);
            assert!(String::from_utf8_lossy(&output.stderr).contains("not found"));
            continue;
        }
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(captured.len(), count + 1);
        let request = captured.last().unwrap().to_string();
        if index == 0 {
            assert!(request.contains("Explicit target current file contents"));
            assert!(request.contains("snapshot-marker-unique"));
            assert!(request.contains("target_kind"));
            assert!(request.contains("high"));
        } else {
            assert!(request.contains("CUSTOM_REVIEW_OVERRIDE custom instructions"));
            assert!(!request.contains("Explicit target current file contents"));
        }
    }
    server.abort();
}
