//! File memories through actual CLI requests and tools, both supported protocols.
#[path = "support/tool_loading.rs"]
mod tool_loading;
use axum::{extract::State, routing::post, Json, Router};
use deepseek_code_core::{
    runtime::RuntimePaths,
    services::{memory_files::FileMemory, EnvironmentCredentials, ServiceContext},
};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
struct Mock {
    protocol: &'static str,
    calls: Vec<(&'static str, Value)>,
    requests: Mutex<Vec<Value>>,
}
struct Server(tokio::task::JoinHandle<()>);
impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn respond(
    State(mock): State<Arc<Mock>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let step = {
        let mut requests = mock.requests.lock().unwrap();
        let step = requests.len();
        requests.push(body);
        step
    };
    let call = mock.calls.get(step);
    let id = format!("memory-call-{step}");
    let events = if mock.protocol == "openai" {
        vec![
            json!({"choices":[{"index":0,"delta":match call {Some((name,input))=>json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]}),None=>json!({"content":"verified"})},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":if call.is_some(){"tool_calls"}else{"stop"}}]}),
        ]
    } else {
        let mut events = vec![
            json!({"type":"message_start","message":{"id":"msg-memory","type":"message","role":"assistant","model":"fixture","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}),
        ];
        if let Some((name, input)) = call {
            events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
            events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
        } else {
            events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
            events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"verified"}}));
        }
        events.extend([json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":if call.is_some(){"tool_use"}else{"end_turn"}},"usage":{"output_tokens":10}}),json!({"type":"message_stop"})]);
        events
    };
    let mut stream = events
        .into_iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<String>();
    if mock.protocol == "openai" {
        stream.push_str("data: [DONE]\n\n");
    }
    ([("content-type", "text/event-stream")], stream)
}
fn note(name: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: 中文测试 fixture preferences\nmetadata:\n  type: user\n---\n{body}\n")
}
async fn run(
    root: &std::path::Path,
    workspace: &std::path::Path,
    base: &str,
    protocol: &str,
    mode: &str,
    resume: Option<&str>,
) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(workspace)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .args([
            "--data-dir",
            root.to_str().unwrap(),
            "--base-url",
            base,
            "--api-format",
            protocol,
            "--api-key-env",
            "MEMORY_FILES_FIXTURE_KEY",
            "--model",
            "fixture",
            "--print",
            "--output-format",
            "stream-json",
            "--permission-mode",
            mode,
        ])
        .env("MEMORY_FILES_FIXTURE_KEY", "local-only")
        .env("NO_PROXY", "127.0.0.1");
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            command.env_remove(key);
        }
    }
    if let Some(id) = resume {
        command.args(["--resume", id]);
    }
    command.args(["--", "中文测试 fixture preferences"]);
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("CLI timeout")
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn first_user_text(request: &Value) -> String {
    let user = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap();
    match &user["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("unexpected user content: {other}"),
    }
}

async fn scenario(protocol: &'static str, plan: bool) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("AGENTS.md"), "project-layout-marker").unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let context = ServiceContext::open(paths.clone(), Arc::new(EnvironmentCredentials)).unwrap();
    let memory = FileMemory::workspace(&context, &workspace).unwrap();
    memory
        .put(
            "preferences.md",
            &note("中文 preferences", "unique-body-recall-marker"),
            None,
        )
        .unwrap();
    let legacy =
        deepseek_code_core::memory::MemExClient::with_path(paths.data_dir.join("memory.sqlite3"));
    legacy
        .ingest(
            "中文测试 legacy-unassigned-must-not-recall",
            Some(0.9),
            None,
        )
        .await
        .unwrap();
    tool_loading::enable(&paths.data_dir, &["ApplyPatch"]);
    let file = memory.directory.join("preferences.md");
    let second = memory.directory.join("second.md");
    let mut calls = vec![
        ("Read", json!({"file_path":file})),
        (
            "Edit",
            json!({"file_path":file,"old_string":"unique-body-recall-marker","new_string":"updated-body-marker"}),
        ),
    ];
    if !plan {
        calls.extend([
        ("Write",json!({"file_path":second,"content":note("Temporary","created-by-model")})),
        ("Read",json!({"file_path":second})),
        ("ApplyPatch",json!({"patch":format!("*** Begin Patch\n*** Delete File: {}\n*** End Patch",second.display())})),
        ("Write",json!({"file_path":workspace.join("outside.txt"),"content":"must ask, denied in dontAsk"})),
    ]);
    }
    let expected_requests = calls.len() + 1;
    let mock = Arc::new(Mock {
        protocol,
        calls,
        requests: Mutex::new(vec![]),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .route("/v1/messages", post(respond))
        .with_state(mock.clone());
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    run(
        &paths.data_dir,
        &workspace,
        &base,
        protocol,
        if plan { "plan" } else { "dontAsk" },
        None,
    )
    .await;
    {
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), expected_requests, "{requests:#?}");
        for request in requests.iter() {
            let system = if protocol == "anthropic" {
                request["system"].to_string()
            } else {
                request["messages"][0]["content"].to_string()
            };
            let first = first_user_text(request);
            assert!(
                !system.contains("<project_memory>")
                    && !system.contains("unique-body-recall-marker")
            );
            let project = first.find("project-layout-marker").unwrap();
            let index = first.find("<project_memory>").unwrap();
            assert!(
                project < index && first[index..].contains("中文 preferences"),
                "{first}"
            );
            assert_eq!(first.matches("Relevant project memory files:").count(), 1);
            assert_eq!(
                first,
                first_user_text(&requests[0]),
                "a tool round changed the first message"
            );
            for message in request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .skip_while(|m| m["role"] != "user")
                .skip(1)
            {
                if message["role"] != "assistant" {
                    assert!(!message["content"]
                        .to_string()
                        .contains("Relevant project memory files:"));
                }
            }
            assert!(!request["tools"].to_string().contains("memory_search"));
            assert!(!request["tools"].to_string().contains("memory_ingest"));
            assert!(!request["messages"]
                .to_string()
                .contains("legacy-unassigned-must-not-recall"));
        }
        assert!(requests[0]["messages"]
            .to_string()
            .contains("unique-body-recall-marker"));
        let last = requests.last().unwrap().to_string();
        assert!(
            last.contains(if plan { "Plan mode" } else { "deleted" }),
            "{last}"
        );
        if !plan {
            assert!(last.contains("saved"), "{last}");
        }
    }
    assert!(!workspace.join("outside.txt").exists());
    assert!(!second.exists());
    let saved = memory
        .store(false)
        .unwrap()
        .read("preferences.md")
        .unwrap()
        .unwrap();
    assert!(saved.document.body.contains(if plan {
        "unique-body-recall-marker"
    } else {
        "updated-body-marker"
    }));
    std::fs::write(
        memory.directory.join("MEMORY.md"),
        "manual-index-edit-on-resume\n",
    )
    .unwrap();
    let id = context.db.latest_code(&workspace).unwrap().unwrap().id;
    run(
        &paths.data_dir,
        &workspace,
        &base,
        protocol,
        if plan { "plan" } else { "dontAsk" },
        Some(&id),
    )
    .await;
    let requests = mock.requests.lock().unwrap();
    let last = requests.last().unwrap();
    assert_eq!(first_user_text(last), first_user_text(&requests[0]));
    assert!(!first_user_text(last).contains("manual-index-edit-on-resume"));
    let latest = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap();
    let text = latest["content"].to_string();
    assert!(
        text.contains("MEMORY.md changed") && text.contains("manual-index-edit-on-resume"),
        "{text}"
    );
    assert!(text.contains("Relevant project memory files:"));
    drop(requests);
    assert_eq!(legacy.stats().await.unwrap().total_memories, 1);
}
#[tokio::test]
async fn file_memory_cli_openai_index_tools_and_resume() {
    scenario("openai", false).await;
}
#[tokio::test]
async fn file_memory_cli_anthropic_index_tools_and_resume() {
    scenario("anthropic", false).await;
}
#[tokio::test]
async fn file_memory_cli_plan_reads_but_cannot_edit() {
    scenario("openai", true).await;
}
