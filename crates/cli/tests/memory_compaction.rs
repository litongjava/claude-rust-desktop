//! Exercise compaction through a resumed CLI and the provider's HTTP interface.
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use deepseek_code_core::{
    db::session_store::Stores,
    runtime::RuntimePaths,
    services::{memory_files::FileMemory, EnvironmentCredentials, ServiceContext},
};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

const QUESTION: &str = "saffron INPUT_MARKER_37";
const ORIGINAL: &str = "ADMISSION_BODY_420";
const EDITED: &str = "RETRY_BODY_780";

fn note(body: &str) -> String {
    format!("---\nname: saffron\ndescription: saffron\nmetadata:\n  type: user\n---\n{body}\n")
}

struct Fixture {
    root: tempfile::TempDir,
    memory: PathBuf,
    id: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let context = ServiceContext::open(paths, Arc::new(EnvironmentCredentials)).unwrap();
        let memory = FileMemory::workspace(&context, &project).unwrap();
        memory.put("saffron.md", &note(ORIGINAL), None).unwrap();
        Self {
            root,
            memory: memory.directory,
            id: uuid::Uuid::new_v4().to_string(),
        }
    }

    async fn serve(
        &self,
        window: u32,
        respond: impl Fn(Value) -> Response + Send + Sync + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let provider = json!([{"id":"fixture","name":"Fixture","base_url":url,"api_key":"fixture-only","api_format":"openai","enabled":true,"models":[{"id":"model","name":"Model","enabled":true,"context_window":window,"max_tokens":1024,"supports_web_search":false,"supports_vision":false}]}]);
        fs::write(
            self.root.path().join("data/providers.json"),
            provider.to_string(),
        )
        .unwrap();
        let respond = Arc::new(respond);
        let endpoint = move |Json(body): Json<Value>| std::future::ready(respond(body));
        let app = Router::new().route("/v1/chat/completions", post(endpoint));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() })
    }

    async fn turn(&self, first: bool, prompt: &str) -> Output {
        let root = self.root.path();
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        cmd.current_dir(root.join("project"))
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .arg("--data-dir")
            .arg(root.join("data"))
            .arg("--coordination-dir")
            .arg(root.join("coord"))
            .args([
                "--provider",
                "fixture",
                "--model",
                "model",
                "--print",
                "--permission-mode",
                "dont-ask",
                if first { "--session-id" } else { "--resume" },
                &self.id,
                "--",
                prompt,
            ]);
        tokio::time::timeout(Duration::from_secs(75), cmd.output())
            .await
            .unwrap()
            .unwrap()
    }
}

fn answer(text: &str) -> Response {
    let chunk = json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]});
    let stop = json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
    (
        [("content-type", "text/event-stream")],
        format!("data: {chunk}\n\ndata: {stop}\n\ndata: [DONE]\n\n"),
    )
        .into_response()
}

fn export(data: &Path, id: &str) -> Vec<Value> {
    let mut out = Vec::new();
    Stores::export_session(data, id, &mut out).unwrap();
    serde_json::Deserializer::from_slice(&out)
        .into_iter()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn admitted(rows: &[Value]) -> Option<(String, Value)> {
    let mut users = rows
        .iter()
        .filter(|r| r["type"] == "message" && r["role"] == "user" && r["content"] == QUESTION);
    let id = users.next()?["id"].as_str().unwrap();
    assert!(users.next().is_none());
    let key = format!("memory.prompt.message.{id}");
    let receipt = rows
        .iter()
        .find(|r| r["type"] == "local_setting" && r["key"] == key)?;
    Some((
        id.to_owned(),
        serde_json::from_str(receipt["value"].as_str().unwrap()).unwrap(),
    ))
}

async fn overflow(fail_summary: bool) {
    let fixture = Fixture::new();
    let id = fixture.id.clone();
    let data = fixture.root.path().join("data");
    let file = fixture.memory.join("saffron.md");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let capture = requests.clone();
    let server = fixture.serve(128_000, move |body| {
        let mut calls = capture.lock().unwrap();
        calls.push((body["messages"].to_string(), admitted(&export(&data, &id))));
        if calls.len() == 2 {
            if !fail_summary { fs::write(&file, note(EDITED)).unwrap(); }
            return (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"context_length_exceeded","message":"prompt is too long"}}))).into_response();
        }
        if body["stream"] != true {
            if fail_summary {
                return (StatusCode::FORBIDDEN, Json(json!({"error":{"message":"summary refused"}}))).into_response();
            }
            return Json(json!({"id":"summary","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"compacted"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})).into_response();
        }
        answer("done")
    }).await;
    let first = fixture.turn(true, "hello").await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let output = fixture.turn(false, QUESTION).await;
    server.abort();
    let _ = server.await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Context limit reached"), "{stderr}");
    assert_eq!(output.status.success(), !fail_summary, "{stderr}");
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), if fail_summary { 3 } else { 4 }, "{stderr}");
    let (id, receipt) = calls[1]
        .1
        .clone()
        .expect("row at the overflowing generation");
    assert!(calls[1].0.contains(QUESTION) && calls[1].0.contains(ORIGINAL));
    assert!(receipt["blocks"].to_string().contains(ORIGINAL));
    assert!(calls[2].0.contains("hello") && !calls[2].0.contains(QUESTION));
    assert_eq!(
        calls[2].1.as_ref(),
        calls[1].1.as_ref(),
        "row and receipt during summary"
    );
    let rows = export(&fixture.root.path().join("data"), &fixture.id);
    let (final_id, final_receipt) = admitted(&rows).expect("admitted row after recovery");
    assert_eq!(final_id, id);
    assert_eq!(final_receipt["blocks"], receipt["blocks"]);
    let messages = rows
        .iter()
        .filter(|r| r["type"] == "message")
        .collect::<Vec<_>>();
    if fail_summary {
        assert!(stderr.contains("403"), "{stderr}");
        assert!(!messages.iter().any(|r| r["is_compact_boundary"] == true));
    } else {
        assert!(stderr.contains("2 messages summarized"), "{stderr}");
        assert_eq!(calls[3].1, calls[1].1);
        assert_eq!(calls[3].0.matches(QUESTION).count(), 1);
        assert_eq!(calls[3].0.matches(ORIGINAL).count(), 1);
        assert!(!calls[3].0.contains(EDITED));
        let at = messages.iter().position(|r| r["id"] == id).unwrap();
        assert_eq!(messages[at - 1]["is_compact_boundary"], true);
    }
}

#[tokio::test]
async fn overflow_retry_reuses_admitted_receipt() {
    overflow(false).await;
}

#[tokio::test]
async fn overflow_summary_failure_keeps_admitted_receipt() {
    overflow(true).await;
}
