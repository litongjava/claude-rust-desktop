//! Real supervisor/host IPC with a local model endpoint and an actual MCP child.
use axum::{extract::State, routing::post, Json, Router};
use claude_core::{
    runtime::{RuntimePaths, SessionStore},
    services::{background_sessions as jobs, session_worker as worker},
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Cleanup(Vec<u32>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for pid in &self.0 {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let _ = std::process::Command::new("taskkill.exe")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .creation_flags(0x08000000)
                    .output();
            }
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .output();
            }
        }
    }
}

async fn respond(
    State((requests, lsp)): State<(Arc<Mutex<Vec<Value>>>, std::path::PathBuf)>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let slow = body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .is_some_and(|s| s.contains("slow"));
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let lsp_turn = last["role"] == "user"
        && last["content"]
            .as_str()
            .is_some_and(|s| s == "first turn" || s == "second turn");
    requests.lock().unwrap().push(body);
    if lsp_turn {
        let arguments=json!({"server":"node","args":[lsp],"filePath":"fixture.rs","operation":"hover","line":1,"character":1}).to_string();
        return (
            [("content-type", "text/event-stream")],
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"lsp-fixture","type":"function","function":{"name":"LSP","arguments":arguments}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
            ),
        );
    }
    if slow {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"worker result"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        ),
    )
}

async fn completed(root: &Path, id: &str) -> jobs::Job {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let job = jobs::get(root, id).unwrap();
            if job.finished_ms.is_some() {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("job did not finish")
}

async fn submit(
    host: &worker::Worker,
    endpoint: &str,
    prompt: &str,
    model: &str,
    max_tokens: u32,
) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let arguments = vec![
        "--data-dir".into(),
        host.root.to_string_lossy().into_owned(),
        "--resume".into(),
        host.session_id.clone(),
        "--base-url".into(),
        format!("{endpoint}/v1"),
        "--api-format".into(),
        "openai".into(),
        "--api-key-env".into(),
        "CLAUDE_RUST_CHILD_API_KEY".into(),
        "--model".into(),
        model.into(),
        "--max-tokens".into(),
        max_tokens.to_string(),
        "--permission-mode".into(),
        "bypass".into(),
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--".into(),
        prompt.into(),
    ];
    let mut environment = vec![(
        "CLAUDE_RUST_CHILD_API_KEY".into(),
        "fixture-secret-only-in-memory".into(),
    )];
    if let Some(base) =
        claude_core::native_engine::provider_manager::one_million_context_base(model)
    {
        environment.push(("CLAUDE_RUST_WORKER_MODEL".into(),json!({"id":base,"name":base,"enabled":true,"supports_vision":false,"supports_web_search":false,"context_window":1_000_000,"max_tokens":max_tokens}).to_string()));
    }
    worker::call(
        host,
        &worker::Request::Submit {
            id: id.clone(),
            prompt: prompt.into(),
            arguments,
            environment,
        },
    )
    .await
    .unwrap();
    id
}

fn assert_no_secret(path: &Path) {
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            assert_no_secret(&path);
        } else {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains("fixture-secret-only-in-memory"),
                "secret persisted in {}",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn persistent_engine_reuses_mcp_refreshes_settings_and_recovers_after_crash_without_replay() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store
        .create(&workspace, "fixture-one", "cli-runtime")
        .unwrap();
    let marker = temp.path().join("starts.txt");
    let script = temp.path().join("mcp.cjs");
    std::fs::write(&script,r#"require('fs').appendFileSync(process.env.START_MARKER,'start\n');require('readline').createInterface({input:process.stdin}).on('line',line=>{const r=JSON.parse(line);if(r.id!==undefined)process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:r.id,result:{protocolVersion:'2024-11-05',capabilities:{},serverInfo:{name:'fixture',version:'1'},tools:[],resources:[]}})+'\n');});"#).unwrap();
    std::fs::write(paths.data_dir.join("mcp-servers.json"),serde_json::to_vec(&json!({"mcpServers":{"fixture":{"command":"node","args":[script],"enabled":true,"env":{"START_MARKER":marker}}}})).unwrap()).unwrap();
    let lsp = temp.path().join("lsp.cjs");
    let lsp_marker = temp.path().join("lsp-starts.txt");
    std::fs::write(workspace.join("fixture.rs"), "fn main() {}\n").unwrap();
    let lsp_script = r#"require('fs').appendFileSync(MARKER,'start\n');let buffer=Buffer.alloc(0);process.stdin.on('data',chunk=>{buffer=Buffer.concat([buffer,chunk]);while(true){let end=buffer.indexOf('\r\n\r\n');if(end<0)return;let length=Number(buffer.subarray(0,end).toString().match(/Content-Length: (\d+)/i)[1]);if(buffer.length<end+4+length)return;let m=JSON.parse(buffer.subarray(end+4,end+4+length));buffer=buffer.subarray(end+4+length);if(m.id){let result=m.method==='initialize'?{capabilities:{}}:m.method==='shutdown'?null:{contents:'persistent hover'};let body=JSON.stringify({jsonrpc:'2.0',id:m.id,result});process.stdout.write('Content-Length: '+Buffer.byteLength(body)+'\r\n\r\n'+body);}if(m.method==='exit')process.exit(0);}});"#;
    std::fs::write(
        &lsp,
        lsp_script.replace("MARKER", &serde_json::to_string(&lsp_marker).unwrap()),
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state((requests.clone(), lsp));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable_copy = temp.path().join(if cfg!(windows) {
        "worker-fixture.exe"
    } else {
        "worker-fixture"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_claude-code"), &executable_copy).unwrap();
    let executable = executable_copy.as_path();
    let host = worker::ensure_worker(&paths.data_dir, executable, &session, &workspace)
        .await
        .unwrap();
    let mut cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
    for (prompt, model, tokens) in [
        ("first turn", "fixture-one", 64),
        ("second turn", "fixture-two[1m]", 96),
    ] {
        let id = submit(&host, &endpoint, prompt, model, tokens).await;
        let job = completed(&paths.data_dir, &id).await;
        assert_eq!(job.status, "completed", "{:?}", job.error);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "start\n");
        let observed = worker::ensure_worker(&paths.data_dir, executable, &session, &workspace)
            .await
            .unwrap();
        assert_eq!(observed.child_pid, host.child_pid);
    }
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[2]["model"], "fixture-two");
        assert_eq!(requests[2]["max_tokens"], 96);
        assert!(requests[2]["messages"].to_string().contains("first turn"));
    }
    assert_eq!(
        std::fs::read_to_string(&lsp_marker).unwrap(),
        "start\n",
        "two tool turns reuse one language server"
    );
    let slow = submit(&host, &endpoint, "slow cancellation", "fixture-two", 96).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while requests.lock().unwrap().len() < 5 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    jobs::cancel(&paths.data_dir, &slow).unwrap();
    assert_eq!(completed(&paths.data_dir, &slow).await.status, "cancelled");
    let next = submit(&host, &endpoint, "after cancellation", "fixture-two", 96).await;
    assert_eq!(completed(&paths.data_dir, &next).await.status, "completed");
    let observed = worker::read_worker(&worker::directory(&host)).unwrap();
    assert_eq!(
        observed.child_pid, host.child_pid,
        "cooperative cancellation must preserve the engine"
    );
    let crash = submit(&host, &endpoint, "slow crash", "fixture-two", 96).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while requests.lock().unwrap().len() < 7 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("taskkill.exe")
            .args(["/PID", &host.child_pid.unwrap().to_string(), "/T", "/F"])
            .creation_flags(0x08000000)
            .output()
            .unwrap();
    }
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-KILL", &host.child_pid.unwrap().to_string()])
            .output()
            .unwrap();
    }
    assert_eq!(completed(&paths.data_dir, &crash).await.status, "failed");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let recovered = worker::ensure_worker(&paths.data_dir, executable, &session, &workspace)
        .await
        .unwrap();
    cleanup.0.push(recovered.supervisor_pid.unwrap());
    assert_ne!(recovered.child_pid, host.child_pid);
    let last = submit(
        &recovered,
        &endpoint,
        "explicit recovery",
        "fixture-two",
        96,
    )
    .await;
    assert_eq!(completed(&paths.data_dir, &last).await.status, "completed");
    assert_eq!(
        requests.lock().unwrap().len(),
        8,
        "no replay after cancellation or crash"
    );
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_no_secret(&paths.data_dir.join("session-workers"));
    assert_no_secret(&paths.data_dir.join("background-tasks"));
    server.abort();
}
