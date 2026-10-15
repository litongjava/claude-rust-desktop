//! Two real workers exchange durable context while a model request is in flight.
use axum::{extract::State, routing::post, Json, Router};
use claude_core::{
    runtime::{RuntimePaths, SessionStore},
    services::{background_sessions as jobs, session_messages as mail, session_worker as worker},
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
struct Fixture {
    recipient: String,
    format: &'static str,
    delegated: bool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    sender_calls: AtomicUsize,
    recipient_calls: AtomicUsize,
    requests: Mutex<Vec<(bool, Value)>>,
}
fn stream(format: &str, tools: Vec<(&str, &str, Value)>) -> String {
    if format == "openai" {
        let tool_mode = !tools.is_empty();
        let delta = if tool_mode {
            json!({"tool_calls":tools.into_iter().enumerate().map(|(index,(id,name,input))|json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})).collect::<Vec<_>>()})
        } else {
            json!({"content":"done"})
        };
        return format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":if tool_mode{"tool_calls"}else{"stop"}}]})
        );
    }
    let mut events = vec![
        json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"model":"fixture","usage":{"input_tokens":1,"output_tokens":0}}}),
    ];
    let tool_mode = !tools.is_empty();
    if tool_mode {
        for (index, (id, name, input)) in tools.into_iter().enumerate() {
            events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
            events.push(json!({"type":"content_block_stop","index":index}));
        }
    } else {
        events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
        events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}));
        events.push(json!({"type":"content_block_stop","index":0}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":if tool_mode{"tool_use"}else{"end_turn"}},"usage":{"output_tokens":2}}));
    events.push(json!({"type":"message_stop"}));
    events
        .into_iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect()
}
async fn respond(
    State(state): State<Arc<Fixture>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let sender = body["messages"].as_array().is_some_and(|messages| {
        messages.iter().any(|message| {
            message["role"] == "user"
                && (message["content"] == "sender task"
                    || message["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|block| block["text"] == "sender task")
                    }))
        })
    });
    state.requests.lock().unwrap().push((sender, body));
    let count = if sender {
        state.sender_calls.fetch_add(1, Ordering::SeqCst)
    } else {
        state.recipient_calls.fetch_add(1, Ordering::SeqCst)
    };
    let tools = if count != 0 {
        Vec::new()
    } else if sender {
        vec![(
            "message",
            "SendMessage",
            json!({"operation":"send","to":state.recipient,"from":"spoofed","content":"LIVE_CONTEXT_MARKER"}),
        )]
    } else if state.delegated {
        vec![(
            "child",
            "Agent",
            json!({"prompt":"sender task", "agent_type":"general"}),
        )]
    } else {
        state.started.notify_one();
        state.release.notified().await;
        vec![
            ("read-one", "Read", json!({"file_path":"fixture.txt"})),
            ("read-two", "Read", json!({"file_path":"fixture.txt"})),
        ]
    };
    (
        [("content-type", "text/event-stream")],
        stream(state.format, tools),
    )
}
async fn submit(host: &worker::Worker, endpoint: &str, format: &str, prompt: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let arguments = [
        "--data-dir",
        host.root.to_str().unwrap(),
        "--resume",
        &host.session_id,
        "--base-url",
        endpoint,
        "--api-format",
        format,
        "--api-key-env",
        "CLAUDE_RUST_CHILD_API_KEY",
        "--model",
        "fixture",
        "--permission-mode",
        "bypass",
        "--print",
        "--output-format",
        "stream-json",
        "--",
        prompt,
    ]
    .map(str::to_owned)
    .to_vec();
    worker::call(
        host,
        &worker::Request::Submit {
            id: id.clone(),
            prompt: prompt.into(),
            arguments,
            environment: vec![(
                "CLAUDE_RUST_CHILD_API_KEY".into(),
                "memory-only-placeholder".into(),
            )],
        },
    )
    .await
    .unwrap();
    id
}
async fn completed(root: &Path, id: &str) -> jobs::Job {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let state = jobs::get(root, id).unwrap();
            if state.finished_ms.is_some() {
                if state.status == "failed" {
                    eprintln!("fixture job failed: {state:?}");
                }
                return state;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap()
}
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
#[tokio::test]
async fn messages_cross_real_workers_at_complete_tool_boundaries_on_both_protocols() {
    for (format, cancel_before_boundary) in
        [("openai", false), ("anthropic", false), ("openai", true)]
    {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("fixture.txt"), "fixture").unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let sender = store.create(&workspace, "fixture", "cli-runtime").unwrap();
        let recipient = store.create(&workspace, "fixture", "cli-runtime").unwrap();
        let state = Arc::new(Fixture {
            recipient: recipient.clone(),
            format,
            delegated: false,
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            sender_calls: AtomicUsize::new(0),
            recipient_calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/v1/chat/completions", post(respond))
            .route("/v1/messages", post(respond))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let executable = temp.path().join(if cfg!(windows) {
            "message-fixture.exe"
        } else {
            "message-fixture"
        });
        std::fs::copy(env!("CARGO_BIN_EXE_claude-code"), &executable).unwrap();
        let target = worker::ensure_worker(&paths.data_dir, &executable, &recipient, &workspace)
            .await
            .unwrap();
        let mut cleanup = Cleanup(vec![target.supervisor_pid.unwrap()]);
        let source = worker::ensure_worker(&paths.data_dir, &executable, &sender, &workspace)
            .await
            .unwrap();
        cleanup.0.push(source.supervisor_pid.unwrap());
        let target_job = submit(&target, &endpoint, format, "recipient task").await;
        tokio::time::timeout(Duration::from_secs(10), state.started.notified())
            .await
            .unwrap();
        let source_job = submit(&source, &endpoint, format, "sender task").await;
        assert_eq!(
            completed(&paths.data_dir, &source_job).await.status,
            "completed"
        );
        let context = mail::context(&paths.data_dir).unwrap();
        let pending = mail::inbox(&context, &recipient, false).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, "queued");
        assert_eq!(pending[0].sender, sender);
        assert_eq!(state.recipient_calls.load(Ordering::SeqCst), 1);
        assert!(mail::receiver_active(&paths.data_dir, &recipient).unwrap());
        if cancel_before_boundary {
            jobs::cancel(&paths.data_dir, &target_job).unwrap();
            assert_eq!(
                completed(&paths.data_dir, &target_job).await.status,
                "cancelled"
            );
            assert_eq!(
                mail::inbox(&context, &recipient, false).unwrap()[0].state,
                "queued"
            );
            assert!(!mail::receiver_active(&paths.data_dir, &recipient).unwrap());
            // Explicit next turn, using recipient's own provider configuration.
            let resumed = submit(&target, &endpoint, format, "recipient resumed explicitly").await;
            assert_eq!(
                completed(&paths.data_dir, &resumed).await.status,
                "completed"
            );
            state.release.notify_one();
        } else {
            state.release.notify_one();
            assert_eq!(
                completed(&paths.data_dir, &target_job).await.status,
                "completed"
            );
        }
        let delivered = mail::inbox(&context, &recipient, false).unwrap();
        assert_eq!(delivered[0].state, "context_committed");
        assert_eq!(state.recipient_calls.load(Ordering::SeqCst), 2);
        let requests = state.requests.lock().unwrap();
        let second = &requests
            .iter()
            .filter(|(sender, _)| !*sender)
            .nth(1)
            .unwrap()
            .1;
        let messages = second["messages"].as_array().unwrap();
        let incoming = messages
            .iter()
            .position(|m| m.to_string().contains("LIVE_CONTEXT_MARKER"))
            .unwrap();
        assert!(incoming > 0);
        assert_eq!(messages[incoming]["role"], "user");
        if cancel_before_boundary {
            assert!(!messages.iter().any(|message| message["role"] == "tool"));
        } else if format == "openai" {
            assert_eq!(messages[incoming - 1]["role"], "tool");
            assert_eq!(messages[incoming - 2]["role"], "tool");
        } else {
            assert!(messages[..incoming].iter().any(|m| m["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))));
        }
        drop(requests);
        assert_eq!(
            store
                .history(&recipient)
                .unwrap()
                .iter()
                .filter(|m| m["content"]
                    .as_str()
                    .is_some_and(|t| t.contains("LIVE_CONTEXT_MARKER")))
                .count(),
            1
        );
        assert!(!mail::receiver_active(&paths.data_dir, &recipient).unwrap());
        worker::shutdown(&paths.data_dir, &recipient).await.unwrap();
        worker::shutdown(&paths.data_dir, &sender).await.unwrap();
        cleanup.0.clear();
        server.abort();
    }
}

#[tokio::test]
async fn managed_worktree_owned_child_can_message_parent_without_new_source_trust() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir(&source).unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@example.invalid"],
        vec!["commit", "--allow-empty", "-qm", "initial"],
    ] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(&source)
            .output()
            .unwrap()
            .status
            .success());
    }
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let parent = store.create(&source, "fixture", "cli-runtime").unwrap();
    let prepared = claude_core::worktree::session::prepare(&source, None, true).unwrap();
    let project = store.resume(&parent).unwrap().project_id.unwrap();
    store
        .db
        .with_conn(|conn| prepared.save(conn, &parent, &project, source.to_str().unwrap()))
        .unwrap()
        .unwrap();
    let state = Arc::new(Fixture {
        recipient: parent.clone(),
        format: "openai",
        delegated: true,
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        sender_calls: AtomicUsize::new(0),
        recipient_calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let executable = temp.path().join(if cfg!(windows) {
        "child-fixture.exe"
    } else {
        "child-fixture"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_claude-code"), &executable).unwrap();
    let host = worker::ensure_worker(&paths.data_dir, &executable, &parent, &prepared.path)
        .await
        .unwrap();
    let mut cleanup = Cleanup(vec![host.supervisor_pid.unwrap()]);
    let job = submit(&host, &endpoint, "openai", "parent task").await;
    assert_eq!(completed(&paths.data_dir, &job).await.status, "completed");
    let context = mail::context(&paths.data_dir).unwrap();
    let inbox = mail::inbox(&context, &parent, false).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].state, "context_committed");
    let child = &inbox[0].sender;
    assert_ne!(child, &parent);
    assert_eq!(
        store.resume(child).unwrap().project_id.as_deref(),
        Some(project.as_str())
    );
    assert_eq!(
        store
            .workspace(child, &paths.data_dir.join("workspaces"))
            .unwrap(),
        store
            .workspace(&parent, &paths.data_dir.join("workspaces"))
            .unwrap()
    );
    assert_eq!(mail::peers(&context, &parent).unwrap().len(), 2);
    assert!(store
        .history(&parent)
        .unwrap()
        .iter()
        .any(|row| row["content"]
            .as_str()
            .is_some_and(|s| s.contains("LIVE_CONTEXT_MARKER"))));
    worker::shutdown(&paths.data_dir, &parent).await.unwrap();
    cleanup.0.clear();
    server.abort();
}
