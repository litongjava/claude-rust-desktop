//! Actual Git + CLI processes exercise source isolation and durable resume.
use axum::{routing::post, Json, Router};
use claude_core::runtime::{RuntimePaths, SessionStore};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

fn git(root: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}

async fn response(Json(body): Json<Value>) -> impl axum::response::IntoResponse {
    let done = body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool";
    let (delta, finish) = if done {
        (json!({"content":"isolated edit complete"}), "stop")
    } else {
        (
            json!({"tool_calls":[{"index":0,"id":"write","type":"function","function":{"name":"Write","arguments":json!({"file_path":"tracked.txt","content":"worktree edit"}).to_string()}}]}),
            "tool_calls",
        )
    };
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
        ),
    )
}

async fn cli(root: &Path, data: &Path, endpoint: &str, options: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_claude-code"));
    command
        .current_dir(root)
        .stdin(Stdio::null())
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--base-url",
            endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "WORKTREE_FIXTURE_KEY",
            "--model",
            "fixture",
            "--permission-mode",
            "bypass",
            "--print",
        ])
        .env("WORKTREE_FIXTURE_KEY", "fixture-only")
        .args(options)
        .args(["--", "edit isolated file"]);
    tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn named_and_generated_worktrees_isolate_dirty_sources_and_resume_after_process_exit() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    git(&source, &["config", "user.name", "Fixture"]);
    git(
        &source,
        &["config", "user.email", "fixture@example.invalid"],
    );
    std::fs::write(source.join("tracked.txt"), "clean").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "initial"]);
    std::fs::write(source.join("tracked.txt"), "source dirty edit").unwrap();
    std::fs::write(source.join("untracked.txt"), "keep").unwrap();
    let extra = root.path().join("extra");
    std::fs::create_dir(&extra).unwrap();
    let second_extra = root.path().join("extra-two");
    std::fs::create_dir(&second_extra).unwrap();
    let before = git(&source, &["status", "--porcelain"]);
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/chat/completions", post(response)),
        )
        .await
        .unwrap()
    });
    let id = uuid::Uuid::new_v4().to_string();
    let first = cli(
        &source,
        &paths.data_dir,
        &endpoint,
        &[
            "--session-id",
            &id,
            "-w",
            "review/issue",
            "--add-dir",
            "../extra",
        ],
    )
    .await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let workspace = store
        .workspace(&id, &paths.data_dir.join("workspaces"))
        .unwrap();
    assert_ne!(workspace, source);
    assert_eq!(
        std::fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "worktree edit"
    );
    assert_eq!(
        git(&workspace, &["branch", "--show-current"]),
        "worktree/review/issue"
    );
    assert_eq!(git(&source, &["status", "--porcelain"]), before);
    assert_eq!(
        std::fs::read_to_string(source.join("tracked.txt")).unwrap(),
        "source dirty edit"
    );
    assert!(workspace.exists(), "exit must retain edits for resume");
    store
        .db
        .with_conn(|conn| {
            let project = claude_core::native_engine::workspace::conversation_project(conn, &id)
                .unwrap()
                .unwrap();
            assert_eq!(project.folders.len(), 2);
            assert_eq!(
                Path::new(&project.folders[1].path).canonicalize().unwrap(),
                extra.canonicalize().unwrap()
            );
        })
        .unwrap();
    let resumed = cli(
        &source,
        &paths.data_dir,
        &endpoint,
        &["--resume", &id, "--add-dir", second_extra.to_str().unwrap()],
    )
    .await;
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        store
            .workspace(&id, &paths.data_dir.join("workspaces"))
            .unwrap(),
        workspace
    );
    assert_eq!(
        store
            .history(&id)
            .unwrap()
            .iter()
            .filter(|row| row["role"] == "user")
            .count(),
        2
    );
    store
        .db
        .with_conn(|conn| {
            assert_eq!(
                claude_core::native_engine::workspace::conversation_project(conn, &id)
                    .unwrap()
                    .unwrap()
                    .folders
                    .len(),
                3
            )
        })
        .unwrap();
    let generated = uuid::Uuid::new_v4().to_string();
    let output = cli(
        &source,
        &paths.data_dir,
        &endpoint,
        &["--session-id", &generated, "--worktree"],
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(
        store
            .workspace(&generated, &paths.data_dir.join("workspaces"))
            .unwrap(),
        workspace
    );
    for name in ["../escape", "/absolute", "bad:name", "review/issue"] {
        let rejected = uuid::Uuid::new_v4().to_string();
        let output = cli(
            &source,
            &paths.data_dir,
            &endpoint,
            &["--session-id", &rejected, "--worktree", name],
        )
        .await;
        assert!(!output.status.success());
        assert!(
            store.resume(&rejected).is_err(),
            "invalid startup must not create an orphan session"
        );
    }
    let nongit = root.path().join("nongit");
    std::fs::create_dir(&nongit).unwrap();
    for initialized in [false, true] {
        if initialized {
            git(&nongit, &["init", "-b", "main"]);
        }
        let rejected = uuid::Uuid::new_v4().to_string();
        let output = cli(
            &nongit,
            &paths.data_dir,
            &endpoint,
            &["--session-id", &rejected, "--worktree"],
        )
        .await;
        assert!(!output.status.success());
        assert!(store.resume(&rejected).is_err());
    }
    assert_eq!(git(&source, &["status", "--porcelain"]), before);
    server.abort();
}
