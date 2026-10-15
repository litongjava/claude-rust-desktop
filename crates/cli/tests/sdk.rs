//! End-to-end SDK tests run the actual CLI process against local model servers.
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
};

#[derive(Clone, Copy)]
enum Mode {
    Text,
    Approval,
    Question,
    Tool,
    SchemaGood,
    SchemaBad,
    SchemaRejected,
}
#[derive(Clone)]
struct Fixture {
    anthropic: bool,
    mode: Mode,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn respond(State(state): State<Fixture>, Json(body): Json<Value>) -> Response {
    let count = {
        let mut requests = state.requests.lock().unwrap();
        requests.push(body);
        requests.len()
    };
    if matches!(state.mode, Mode::SchemaRejected) {
        return (
            StatusCode::BAD_REQUEST,
            "fixture: structured output unsupported",
        )
            .into_response();
    }
    let text = match state.mode {
        Mode::SchemaGood => r#"{"answer":42}"#.into(),
        Mode::SchemaBad => r#"{"answer":"wrong"}"#.into(),
        _ => format!("reply-{count}"),
    };
    let tool = matches!(state.mode, Mode::Tool)
        || matches!(state.mode, Mode::Approval | Mode::Question) && count % 2 == 1;
    let stream = if state.anthropic {
        let mut frames = vec![
            json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"model":"fixture","usage":{"input_tokens":10,"output_tokens":0}}}),
        ];
        if tool {
            let (id, name, args) = if matches!(state.mode, Mode::Question) {
                (
                    "question-1",
                    "AskUserQuestion",
                    json!({"question":"Which language?","description":"Choose the implementation language","multiSelect":true,"options":[{"label":"Rust","description":"Memory safety"},{"label":"Go","description":"Fast compilation"}]}),
                )
            } else {
                ("read-1", "Read", json!({"file_path":"fixture.txt"}))
            };
            frames.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":args.to_string()}})]);
        } else {
            frames.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})]);
        }
        frames.extend([json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":4}}),json!({"type":"message_stop"})]);
        frames
            .into_iter()
            .map(|frame| {
                format!(
                    "event: {}\ndata: {frame}\n\n",
                    frame["type"].as_str().unwrap()
                )
            })
            .collect::<String>()
    } else {
        let delta = if tool {
            if matches!(state.mode, Mode::Question) {
                json!({"tool_calls":[{"index":0,"id":"question-1","type":"function","function":{"name":"AskUserQuestion","arguments":json!({"question":"Which language?","description":"Choose the implementation language","multiSelect":true,"options":[{"label":"Rust","description":"Memory safety"},{"label":"Go","description":"Fast compilation"}]}).to_string()}}]})
            } else if matches!(state.mode, Mode::Approval) {
                json!({"tool_calls":[{"index":0,"id":"write-1","type":"function","function":{"name":"Write","arguments":json!({"file_path":"approval.txt","content":"ACTUAL_APPROVED_WRITE"}).to_string()}}]})
            } else {
                json!({"tool_calls":[{"index":0,"id":"read-1","type":"function","function":{"name":"Read","arguments":"{\"file_path\":\"fixture.txt\"}"}}]})
            }
        } else {
            json!({"content":text})
        };
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":if tool {"tool_calls"} else {"stop"}}]})
        )
    };
    ([("content-type", "text/event-stream")], stream).into_response()
}

struct Server {
    url: String,
    fixture: Fixture,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(anthropic: bool, mode: Mode) -> Self {
        let fixture = Fixture {
            anthropic,
            mode,
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                if anthropic {
                    "/v1/messages"
                } else {
                    "/v1/chat/completions"
                },
                post(respond),
            )
            .with_state(fixture.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, fixture, task }
    }
}

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: Lines<BufReader<ChildStdout>>,
}
impl Client {
    async fn frame(&mut self, kind: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .unwrap()
                    .expect("CLI ended before expected SDK frame");
                let frame: Value = serde_json::from_str(&line).unwrap();
                assert!(frame["type"]!="result" || kind=="result","Unexpected terminal result while waiting for {kind}: {frame}");
                if frame["type"] == kind {
                    return frame;
                }
            }
        })
        .await
        .unwrap()
    }
    fn start(root: &Path, server: &Server, extra: &[&str]) -> Self {
        std::fs::write(root.join("fixture.txt"), "SDK_READ_SENTINEL").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_claude-code"));
        command
            .args([
                "--data-dir",
                root.join("data").to_str().unwrap(),
                "--path",
                root.to_str().unwrap(),
                "--base-url",
                &server.url,
                "--api-format",
                if server.fixture.anthropic {
                    "anthropic"
                } else {
                    "openai"
                },
                "--api-key-env",
                "CLAUDE_SDK_FIXTURE_KEY",
                "--model",
                "fixture-model",
                "--output-format",
                "stream-json",
                "--permission-mode",
                "accept-edits",
                "--print",
                "--input-format",
                "stream-json",
            ])
            .args(extra);
        command
            .env("CLAUDE_SDK_FIXTURE_KEY", "fixture-only")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "API_BASE_URL",
            "ANTHROPIC_BASE_URL",
            "OPENAI_BASE_URL",
            "CLAUDE_MODEL",
            "RUST_LOG",
        ] {
            command.env_remove(key);
        }
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            input,
            output,
        }
    }
    async fn send(&mut self, value: Value) {
        self.raw(&format!("{value}\n")).await;
    }
    async fn raw(&mut self, text: &str) {
        let input = self.input.as_mut().unwrap();
        input.write_all(text.as_bytes()).await.unwrap();
        input.flush().await.unwrap();
    }
    async fn result(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .unwrap()
                    .expect("CLI ended before a result");
                let frame: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| panic!("Non-JSON stdout: {line}"));
                if frame["type"] == "result" {
                    return frame;
                }
            }
        })
        .await
        .expect("CLI did not emit a result while stdin stayed open")
    }
    async fn finish(self) -> (std::process::ExitStatus, Vec<Value>) {
        let (status, results, _) = self.finish_with_transcript().await;
        (status, results)
    }
    async fn finish_with_transcript(mut self) -> (std::process::ExitStatus, Vec<Value>, String) {
        self.input.take();
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut results = Vec::new();
            let mut transcript = String::new();
            while let Some(line) = self.output.next_line().await.unwrap() {
                transcript.push_str(&line);
                transcript.push('\n');
                let frame: Value = serde_json::from_str(&line).unwrap();
                if frame["type"] == "result" {
                    results.push(frame);
                }
            }
            (self.child.wait().await.unwrap(), results, transcript)
        })
        .await
        .expect("CLI failed to exit after stdin EOF")
    }
}

fn user(text: &str) -> Value {
    json!({"type":"user","message":{"role":"user","content":text}})
}

#[tokio::test]
async fn sdk_question_answers_reach_model_and_history_without_rewriting_questions() {
    for (outcome, anthropic) in [
        ("answer", false),
        ("answer", true),
        ("changed", false),
        ("deny", false),
        ("interrupt", false),
        ("eof", false),
    ] {
        let server = Server::start(anthropic, Mode::Question).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(user("Ask my preference")).await;
        let request = client.frame("control_request").await;
        assert_eq!(request["request"]["tool_name"], "AskUserQuestion");
        assert_eq!(request["request"]["tool_use_id"], "question-1");
        assert_eq!(
            request["request"]["input"]["questions"][0]["multiSelect"],
            true
        );
        assert_eq!(
            request["request"]["input"]["questions"][0]["options"][0]["description"],
            "Memory safety"
        );
        assert_eq!(
            request["request"]["description"],
            "Choose the implementation language"
        );
        if outcome == "eof" {
            let (status, results) = client.finish().await;
            assert!(!status.success());
            assert_eq!(results.len(), 1);
            assert_eq!(results[0]["is_error"], true);
            continue;
        }
        if outcome == "interrupt" {
            client.send(json!({"type":"control_request","request_id":"cancel-question","request":{"subtype":"interrupt"}})).await;
            client.frame("control_response").await;
        } else {
            let mut updated = request["request"]["input"].clone();
            updated["answers"] = json!({"Which language?":"Rust 中文😀 and custom free text"});
            if outcome == "changed" {
                updated["questions"][0]["question"] = json!("Injected replacement");
            }
            client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":{"behavior":if outcome=="deny" {"deny"} else {"allow"},"updatedInput":updated}}})).await;
        }
        let result = client.result().await;
        assert_eq!(result["is_error"], outcome != "answer", "{result}");
        let (status, _) = client.finish().await;
        assert_eq!(status.success(), outcome == "answer");
        let requests = server.fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), if outcome == "answer" { 2 } else { 1 });
        if outcome == "answer" {
            assert!(requests[1]["messages"]
                .to_string()
                .contains("Rust 中文😀 and custom free text"));
            let paths = claude_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
            let store = claude_core::runtime::SessionStore::new(paths.open_database().unwrap());
            let history = store
                .model_history(
                    result["session_id"].as_str().unwrap(),
                    &claude_core::native_engine::provider_manager::ApiFormat::OpenAI,
                )
                .unwrap();
            assert!(serde_json::to_string(&history)
                .unwrap()
                .contains("Rust 中文😀 and custom free text"));
        }
    }
}

#[tokio::test]
async fn embedded_images_reach_both_providers_and_survive_history_and_fork() {
    const PNG:&str="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY9sAAAAASUVORK5CYII=";
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let paths = claude_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
        let mut config = claude_core::config::AppConfig::default();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        config.providers.push(serde_json::from_value(json!({"id":"vision-fixture","name":"Vision fixture","provider_type":"openai","base_url":server.url,"enabled":true,"is_default":false,"settings":{},"supports_web_search":false,"models":[{"id":"fixture-model","name":"fixture-model","enabled":true,"supports_vision":true,"supports_tools":true,"supports_text":true,"supports_streaming":true}]})).unwrap());
        std::fs::write(
            paths.data_dir.join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"Before image"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}},{"type":"text","text":"After image"}]}})).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{result}");
        client.send(user("Remember that image")).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(client.finish().await.0.success());
        let requests = server.fixture.requests.lock().unwrap();
        for request in requests.iter() {
            assert!(request["messages"].to_string().contains(PNG));
            let content = &request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "user" && m["content"].is_array())
                .unwrap()["content"];
            assert_eq!(content[0]["text"], "Before image");
            assert_eq!(content[2]["text"], "After image");
            if anthropic {
                assert_eq!(content[1]["source"]["data"], PNG);
            } else {
                assert!(content[1]["image_url"]["url"]
                    .as_str()
                    .unwrap()
                    .ends_with(PNG));
            }
        }
        drop(requests);
        let store = claude_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let id = result["session_id"].as_str().unwrap();
        let fork = store.fork(id).unwrap();
        let history = store
            .model_history(
                &fork,
                &claude_core::native_engine::provider_manager::ApiFormat::OpenAI,
            )
            .unwrap();
        assert!(history[0].to_string().contains(PNG));
        assert_eq!(history[0]["content"][2]["text"], "After image");
    }
}

#[tokio::test]
async fn controls_replay_and_live_permission_decisions_use_actual_engine() {
    for (allow, mutate) in [(false, false), (true, false), (true, true)] {
        let server = Server::start(false, Mode::Approval).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(
            root.path(),
            &server,
            &["--permission-mode", "ask", "--replay-user-messages"],
        );
        client.send(json!({"type":"control_request","request_id":"init","request":{"subtype":"initialize"}})).await;
        assert_eq!(
            client.frame("control_response").await["response"]["subtype"],
            "success"
        );
        client.send(json!({"type":"control_request","request_id":"model","request":{"subtype":"set_model","model":"fixture-model"}})).await;
        assert_eq!(
            client.frame("control_response").await["response"]["subtype"],
            "success"
        );
        let mut message = user("Write approval.txt");
        message["uuid"] = json!("retained-user-uuid");
        client.send(message).await;
        assert_eq!(client.frame("user").await["uuid"], "retained-user-uuid");
        let permission = client.frame("control_request").await;
        assert_eq!(permission["request"]["tool_name"], "Write");
        assert_eq!(permission["request"]["tool_use_id"], "write-1");
        assert!(!root.path().join("approval.txt").exists());
        let mut updated = permission["request"]["input"].clone();
        if mutate {
            updated["content"] = json!("Changed unreviewed content");
        }
        client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":permission["request_id"],"response":{"behavior":if allow {"allow"} else {"deny"},"updatedInput":updated}}})).await;
        assert_eq!(client.result().await["is_error"], mutate);
        if mutate {
            assert!(!root.path().join("approval.txt").exists());
            assert!(!client.finish().await.0.success());
            continue;
        }
        assert_eq!(root.path().join("approval.txt").exists(), allow);
        if allow {
            assert_eq!(
                std::fs::read_to_string(root.path().join("approval.txt")).unwrap(),
                "ACTUAL_APPROVED_WRITE"
            );
        }
        client.send(json!({"type":"control_request","request_id":"mode","request":{"subtype":"set_permission_mode","mode":"acceptEdits"}})).await;
        assert_eq!(
            client.frame("control_response").await["response"]["subtype"],
            "success"
        );
        client
            .send(user("Write with updated permission mode"))
            .await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(root.path().join("approval.txt").exists());
        assert!(client.finish().await.0.success());
    }
}

#[tokio::test]
async fn interrupt_cancels_pending_approval_and_session_accepts_next_message() {
    let server = Server::start(false, Mode::Approval).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
    client.send(user("Write a file")).await;
    let pending = client.frame("control_request").await;
    client
        .send(
            json!({"type":"control_request","request_id":"stop","request":{"subtype":"interrupt"}}),
        )
        .await;
    assert_eq!(
        client.frame("control_cancel_request").await["request_id"],
        pending["request_id"]
    );
    assert_eq!(
        client.frame("control_response").await["response"]["request_id"],
        "stop"
    );
    assert_eq!(client.result().await["is_error"], true);
    assert!(!root.path().join("approval.txt").exists());
    client.send(user("Continue without tools")).await;
    assert_eq!(client.result().await["result"], "reply-2");
    assert!(!client.finish().await.0.success());
}

#[tokio::test]
async fn sdk_eof_during_approval_and_unsupported_initialization_fail_explicitly() {
    let server = Server::start(false, Mode::Approval).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
    client.send(json!({"type":"control_request","request_id":"unsupported","request":{"subtype":"initialize","hooks":{"PreToolUse":[{"hookCallbackIds":["callback"]}]}}})).await;
    let response = client.frame("control_response").await;
    assert_eq!(response["response"]["subtype"], "error");
    assert!(response["response"]["error"]
        .as_str()
        .unwrap()
        .contains("hooks"));
    client.send(user("Write pending approval")).await;
    client.frame("control_request").await;
    let (status, results) = client.finish().await;
    assert!(!status.success());
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["is_error"], true);
    assert!(!root.path().join("approval.txt").exists());
}

#[tokio::test]
async fn streaming_process_handles_two_messages_before_eof_in_one_session() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(user("first")).await;
        let first = client.result().await;
        assert_eq!(first["result"], "reply-1");
        assert_eq!(first["is_error"], false);
        let mut second = user("second");
        second["session_id"] = first["session_id"].clone();
        client.send(second).await;
        let second = client.result().await;
        assert_eq!(second["result"], "reply-2");
        assert_eq!(second["session_id"], first["session_id"]);
        let (status, extra) = client.finish().await;
        assert!(status.success());
        assert!(extra.is_empty(), "Duplicate result: {extra:?}");
        let requests = server.fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1]["messages"].to_string().contains("first"));
    }
}

#[tokio::test]
async fn initial_print_prompt_runs_before_streamed_messages() {
    let server = Server::start(false, Mode::Text).await;
    let root = tempfile::tempdir().unwrap();
    let mut client = Client::start(root.path(), &server, &["-p", "initial prompt"]);
    let first = client.result().await;
    assert_eq!(first["result"], "reply-1");
    client.send(user("streamed follow-up")).await;
    let second = client.result().await;
    assert_eq!(second["result"], "reply-2");
    assert_eq!(second["session_id"], first["session_id"]);
    let (status, extra) = client.finish().await;
    assert!(status.success());
    assert!(extra.is_empty());
}

#[tokio::test]
async fn invalid_schema_fails_before_any_model_request() {
    for schema in [
        "invalid JSON",
        r#"{"type":42}"#,
        r#"{"$ref":"https://example.invalid/schema"}"#,
    ] {
        let server = Server::start(false, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let client = Client::start(root.path(), &server, &["--json-schema", schema]);
        let (status, results) = client.finish().await;
        assert!(!status.success());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["is_error"], true);
        assert!(server.fixture.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn malformed_frame_or_wrong_session_produces_one_failure() {
    for wrong_session in [false, true] {
        let server = Server::start(false, Mode::Text).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(user("valid")).await;
        let first = client.result().await;
        assert_eq!(first["is_error"], false);
        if wrong_session {
            let mut frame = user("wrong");
            frame["session_id"] = json!("not-the-active-session");
            client.send(frame).await;
        } else {
            client.raw("{malformed}\n").await;
        }
        let (status, results) = client.finish().await;
        assert!(!status.success());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["is_error"], true);
        assert_eq!(server.fixture.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn max_turns_executes_tool_then_fails_once_without_another_model_request() {
    for anthropic in [false, true] {
        let server = Server::start(anthropic, Mode::Tool).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = Client::start(root.path(), &server, &["--max-turns", "1"]);
        client.send(user("Read fixture.txt then answer")).await;
        let (status, results, transcript) = client.finish_with_transcript().await;
        assert!(
            transcript.contains("SDK_READ_SENTINEL"),
            "The CLI must execute the actual Read tool"
        );
        assert!(!status.success());
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0]["is_error"], true);
        assert!(results[0]["error"]
            .as_str()
            .unwrap()
            .contains("Max tool iterations"));
        assert_eq!(server.fixture.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn structured_results_are_enforced_and_provider_rejection_is_not_softened() {
    let schema=json!({"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"],"additionalProperties":false}).to_string();
    for anthropic in [false, true] {
        for mode in [Mode::SchemaGood, Mode::SchemaBad, Mode::SchemaRejected] {
            let server = Server::start(anthropic, mode).await;
            let root = tempfile::tempdir().unwrap();
            let mut client = Client::start(root.path(), &server, &["--json-schema", &schema]);
            client.send(user("Return an answer")).await;
            let (status, results) = client.finish().await;
            assert_eq!(results.len(), 1, "Duplicate/missing result: {results:?}");
            if matches!(mode, Mode::SchemaGood) {
                assert!(status.success());
                assert_eq!(results[0]["structured_output"], json!({"answer":42}));
            } else {
                assert!(!status.success());
                assert_eq!(results[0]["is_error"], true);
                assert!(results[0]["structured_output"].is_null());
            }
            let requests = server.fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let actual = if anthropic {
                &requests[0]["output_config"]["format"]["schema"]
            } else {
                &requests[0]["response_format"]["json_schema"]["schema"]
            };
            assert_eq!(actual, &serde_json::from_str::<Value>(&schema).unwrap());
        }
    }
}
