//! Real standalone server and service host executables, using isolated data directories.
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Output, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

fn cli(path: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_claude-code"));
    command
        .arg("--data-dir")
        .arg(path)
        .env("TEAM_SYNC_TOKEN", "process-token-long-enough")
        .kill_on_drop(true);
    command
}
async fn output(path: &Path, args: &[&str]) -> Output {
    tokio::time::timeout(Duration::from_secs(25), cli(path).args(args).output())
        .await
        .unwrap()
        .unwrap()
}
async fn run(path: &Path, args: &[&str]) -> Value {
    let result = output(path, args).await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

#[tokio::test]
async fn personal_memory_transfer_is_standalone_and_clear_requires_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("legacy.json");
    let export = dir.path().join("export.json");
    let bytes = serde_json::to_vec(&json!([{"id":"legacy", "memory_type":"Preference", "content":"Prefer Chinese", "timestamp":"2026-10-15T00:00:00Z", "importance":0.7, "tags":[], "metadata":{}}])).unwrap();
    std::fs::write(&source, &bytes).unwrap();
    let input = source.to_str().unwrap();
    assert_eq!(run(dir.path(), &["memory","import",input,"--preview"]).await["imported"], 1);
    assert_eq!(run(dir.path(), &["memory","status"]).await["total_memories"], 0);
    assert_eq!(run(dir.path(), &["memory","import",input]).await["imported"], 1);
    assert_eq!(run(dir.path(), &["memory","import",input]).await["skipped"], 1);
    assert_eq!(run(dir.path(), &["memory","export",export.to_str().unwrap()]).await["exported"], 1);
    assert!(!output(dir.path(), &["memory","export",export.to_str().unwrap()]).await.status.success());
    assert!(!output(dir.path(), &["memory","clear"]).await.status.success());
    run(dir.path(), &["memory","clear","--yes"]).await;
    run(dir.path(), &["memory","import",export.to_str().unwrap()]).await;
    assert_eq!(run(dir.path(), &["memory","status"]).await["total_memories"], 1);
    assert_eq!(std::fs::read(source).unwrap(), bytes);
}
async fn start_server(path: &Path) -> (String, Child) {
    let mut child = cli(path)
        .args(["team-sync", "serve"])
        .env(
            "TEAM_SERVER_TOKENS",
            json!({"process-token-long-enough":"alpha"}).to_string(),
        )
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let value: Value = serde_json::from_str(&line).unwrap();
    (
        format!("http://{}", value["listening"].as_str().unwrap()),
        child,
    )
}
#[tokio::test]
async fn separate_processes_share_host_controls_and_sync_without_desktop() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    let remote = dir.path().join("remote");
    let (url, mut server) = start_server(&remote).await;
    for path in [&a, &b] {
        run(path, &["team-sync", "auth", "alpha", "--endpoint", &url]).await;
    }
    let record = run(
        &a,
        &[
            "team-sync",
            "create",
            "Decision",
            "--content",
            "Use Rust",
            "--tags",
            "architecture",
        ],
    )
    .await;
    let mut host = cli(&a)
        .args(["services", "start", "--interval", "1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = run(&a, &["services", "status"]).await;
            if status
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "team-sync" && s["completed"].as_u64().unwrap() > 0)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let duplicate = output(&a, &["services", "start"]).await;
    assert!(!duplicate.status.success());
    run(&b, &["team-sync", "sync"]).await;
    let list = run(&b, &["team-sync", "list"]).await;
    assert_eq!(list[0]["content"], "Use Rust");
    run(&a, &["services", "stop"]).await;
    assert!(tokio::time::timeout(Duration::from_secs(3), host.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    run(&a, &["team-sync", "delete", record["id"].as_str().unwrap()]).await;
    run(&a, &["team-sync", "sync"]).await;
    run(&b, &["team-sync", "sync"]).await;
    assert!(run(&b, &["team-sync", "list"])
        .await
        .as_array()
        .unwrap()
        .is_empty());
    server.kill().await.unwrap();
    server.wait().await.unwrap();
    let offline = output(&a, &["team-sync", "sync"]).await;
    assert!(!offline.status.success());
}

#[tokio::test]
async fn voice_file_is_audio_upload_and_can_be_sent_to_shared_engine_session() {
    use axum::{routing::post, Json, Router};
    let received = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let captured = received.clone();
    let router=Router::new().route("/v1/audio/transcriptions",post(move|mut multipart:axum::extract::Multipart| {let captured=captured.clone();async move {
        while let Some(field)=multipart.next_field().await.unwrap(){if field.name()==Some("file"){*captured.lock().await=field.bytes().await.unwrap().to_vec();}}
        Json(json!({"text":"recognized coding request"}))
    }})).route("/v1/chat/completions",post(|Json(body):Json<Value>|async move {
        assert_eq!(body["messages"].as_array().unwrap().last().unwrap()["content"],"recognized coding request");
        let chunks=format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":"shared engine replied"},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
        ([("content-type","text/event-stream")],chunks)
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let file = dir.path().join("recording.wav");
    let audio = b"RIFF recorded samples";
    std::fs::write(&file, audio).unwrap();
    run(
        &data,
        &[
            "voice-config",
            &format!("{endpoint}/v1/audio/transcriptions"),
            "--credential",
            "TEAM_SYNC_TOKEN",
        ],
    )
    .await;
    let result = run(&data, &["voice", "--file", file.to_str().unwrap()]).await;
    assert_eq!(result["text"], "recognized coding request");
    assert_eq!(*received.lock().await, audio);
    let result = run(
        &data,
        &[
            "--path",
            dir.path().to_str().unwrap(),
            "--base-url",
            &endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "TEAM_SYNC_TOKEN",
            "--model",
            "test-model",
            "--permission-mode",
            "plan",
            "--output-format",
            "json",
            "voice",
            "--file",
            file.to_str().unwrap(),
            "--send",
        ],
    )
    .await;
    assert_eq!(result["result"], "shared engine replied");
    let sessions = run(&data, &["sessions", "list"]).await;
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    server.abort();
    let _ = server.await;
}
