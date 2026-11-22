//! Compare the history seen by the main and compaction requests with durable rows.
use axum::{http::HeaderMap, response::IntoResponse, routing::post, Json, Router};
use deepseek_code_core::{
    cache::FileCache,
    native_engine::{
        engine_core::ChatRequest,
        provider_manager::{Provider, ProviderManager},
    },
    runtime::{self, RuntimePaths, SessionStore},
    services::compaction,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[path = "../src/native_engine/provider_runtime/bedrock.rs"]
mod bedrock;

struct Fixture {
    root: tempfile::TempDir,
    paths: RuntimePaths,
    store: SessionStore,
    id: String,
    provider: Provider,
    requests: Arc<Mutex<Vec<Value>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(window: usize) -> Self {
        Self::with_root(window, tempfile::tempdir().unwrap()).await
    }

    async fn with_root(window: usize, root: tempfile::TempDir) -> Self {
        deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(root.path(), "model", "fixture").unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let app = Router::new().fallback(post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let compact = headers["x-dcl-request-kind"] == "compaction";
            let stream = body["stream"] == true;
            let responses = body.get("input").is_some();
            let bedrock = body.get("inferenceConfig").is_some();
            let anthropic = headers.contains_key("anthropic-version");
            let structured = body["response_format"]["type"] == "json_schema" || body["messages"].to_string().contains("fixture always answers JSON");
            let text = if structured { r#"{"ok":true}"# } else if compact { "Pending work is recorded." } else { "last answer" };
            seen.lock().unwrap().push(body);
            async move {
                let usage = json!({"prompt_tokens":100,"completion_tokens":3,"total_tokens":103,"prompt_tokens_details":{"cached_tokens":80}});
                if bedrock {
                    let frames: Vec<u8> = [("messageStart",json!({"role":"assistant"})),
                        ("contentBlockDelta",json!({"contentBlockIndex":0,"delta":{"text":text}})),
                        ("messageStop",json!({"stopReason":"end_turn"})),
                        ("metadata",json!({"usage":{"inputTokens":100,"outputTokens":3,"totalTokens":103}}))]
                        .into_iter().flat_map(|(kind, payload)| bedrock::frame(kind, payload)).collect();
                    ([("content-type","application/vnd.amazon.eventstream")],frames).into_response()
                } else if anthropic {
                    let frames = [json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":100}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
                        json!({"type":"message_stop"})].iter().map(|v|format!("data: {v}\n\n")).collect::<String>();
                    ([("content-type","text/event-stream")],frames).into_response()
                } else if responses {
                    let delta = json!({"type":"response.output_text.delta","delta":text});
                    let done = json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],"usage":{"input_tokens":100,"output_tokens":3}}});
                    ([("content-type","text/event-stream")],format!("data: {delta}\n\ndata: {done}\n\n")).into_response()
                } else if stream {
                    let delta = json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}],"usage":usage});
                    ([("content-type","text/event-stream")],format!("data: {delta}\n\ndata: [DONE]\n\n")).into_response()
                } else {
                    Json(json!({"id":"summary","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":usage})).into_response()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = serde_json::from_value(json!({"id":"fixture","name":"fixture","api_format":"openai","api_key":"fixture","base_url":format!("http://{addr}"),"enabled":true,"models":[{"id":"model","name":"fixture","enabled":true,"max_tokens":4096,"context_window":window,"supports_web_search":false}]})).unwrap();
        Self {
            root,
            paths,
            store,
            id,
            provider,
            requests,
            server,
        }
    }

    fn engine(&self) -> deepseek_code_core::native_engine::engine_core::NativeEngine {
        runtime::create_engine(
            &self.paths,
            self.store.db.clone(),
            Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
                vec![self.provider.clone()],
                self.paths.data_dir.join("providers.json"),
            ))),
            runtime::default_permissions(),
            Arc::new(FileCache::new()),
        )
    }

    fn append(&self, role: &str, text: &str) {
        self.store.append(&self.id, role, text).unwrap();
    }

    async fn main_turn(&self, schema: Option<Value>) {
        let request: ChatRequest = serde_json::from_value(json!({"conversation_id":self.id,
            "messages":self.store.model_history(&self.id,&self.provider.api_format).unwrap(),
            "response_schema":schema,"system_prompt":"Custom session instructions","model":self.provider.models[0].id,"effort":self.provider.models[0].effort,"provider_id":"fixture","workspace_path":self.root.path()})).unwrap();
        let mut stream = self
            .engine()
            .send_message_with_history(request, false)
            .await
            .unwrap();
        while let Some(event) = stream.recv().await {
            assert!(
                !matches!(
                    event,
                    deepseek_code_core::native_engine::tool_loop::EngineEvent::Error(_)
                ),
                "{event:?}"
            );
        }
        self.append(
            "assistant",
            if schema.is_some() {
                r#"{"ok":true}"#
            } else {
                "last answer"
            },
        );
    }

    async fn compact(&self) -> anyhow::Result<compaction::CompactionResult> {
        compaction::compact(
            &self.engine(),
            &self.store.db,
            &self.id,
            "fixture",
            &self.provider.models[0].id,
            "Keep pending work",
        )
        .await
    }

    async fn finish(self) {
        self.server.abort();
        let _ = self.server.await;
    }
}

#[tokio::test]
async fn shortened_main_turn_summarizes_the_complete_durable_input() {
    let f = Fixture::new(50_000).await;
    let marker = "ORIGINAL_KEEP_MARKER";
    f.append("user", &format!("{}{marker}", "w".repeat(100_000)));
    for i in 0..5 {
        f.append("assistant", &format!("earlier answer {i}"));
        f.append("user", &format!("later question {i}"));
    }
    f.main_turn(None).await;
    f.compact().await.unwrap();
    {
        let calls = f.requests.lock().unwrap();
        assert!(!calls[0].to_string().contains(marker));
        assert!(calls[1..]
            .iter()
            .any(|body| body.to_string().contains(marker)));
    }
    f.finish().await;
}

#[tokio::test]
async fn fallback_after_a_text_boundary_includes_the_retained_originals() {
    let f = Fixture::new(128_000).await;
    f.append("user", "Original requirement preceding the boundary");
    f.append("assistant", "Acknowledged the original requirement.");
    let prepared = compaction::prepare(&f.store.db, &f.id).unwrap();
    compaction::commit(&f.store.db, prepared, "Prior continuation summary").unwrap();
    f.append("user", "New requirement following the boundary");
    f.compact().await.unwrap();
    {
        let calls = f.requests.lock().unwrap();
        let body = calls[0].to_string();
        for expected in [
            "Original requirement preceding the boundary",
            "Prior continuation summary",
            "New requirement following the boundary",
        ] {
            assert!(body.contains(expected));
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn opaque_fallback_sends_native_items_and_the_retained_transcript() {
    check_opaque_fallback(128_000, "").await;
}

#[tokio::test]
async fn segmented_fallback_preserves_the_window_in_each_request() {
    check_opaque_fallback(8192, &"w".repeat(40_000)).await;
}

async fn check_opaque_fallback(window: usize, padding: &str) {
    let mut f = Fixture::new(window).await;
    f.provider.builtin_id = Some("openai".into());
    f.provider.models[0].api = Some("openai-responses".into());
    f.append("user", &format!("Original durable requirement {padding}"));
    let boundary = f
        .store
        .append(&f.id, "user", "Native Responses compaction completed.")
        .unwrap();
    let items = json!([{"type":"compaction","id":"original","encrypted_content":"OPAQUE_WINDOW"}]);
    f.store.db.open_session(&f.id).unwrap().write(|w| w.native_boundary(&boundary,
        &json!({"compaction":true,"api":"openai-responses","items":items,"provider":"fixture","model":"model","base_url":f.provider.base_url}))).unwrap();
    f.append("user", "Durable correction after boundary");
    f.compact().await.unwrap();
    {
        let calls = f.requests.lock().unwrap();
        assert!(!calls.is_empty());
        if !padding.is_empty() {
            assert!(calls.len() > 1);
        }
        assert!(calls.iter().all(|body| body["input"]
            .as_array()
            .is_some_and(|input| input.contains(&items[0]))));
        for expected in [
            "Original durable requirement",
            "Durable correction after boundary",
        ] {
            assert!(calls.iter().any(|body| body.to_string().contains(expected)));
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn schema_constrained_main_turn_requires_a_text_summary_before_commit() {
    for stubborn in [false, true] {
        let f = Fixture::new(128_000).await;
        f.append(
            "user",
            if stubborn {
                "fixture always answers JSON"
            } else {
                "Continue work"
            },
        );
        f.main_turn(Some(json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}))).await;
        let before = f.store.history(&f.id).unwrap();
        let result = f.compact().await;
        let after = f.store.history(&f.id).unwrap();
        {
            let requests = f.requests.lock().unwrap();
            assert_eq!(requests[0]["response_format"]["type"], "json_schema");
            assert!(requests[1].get("response_format").is_none());
            assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        }
        if stubborn {
            assert!(result.is_err());
            assert_eq!(before, after);
        } else {
            result.unwrap();
            assert!(after.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .ends_with("Pending work is recorded."));
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn first_iteration_peer_delivery_is_present_once_in_the_reused_prefix() {
    use deepseek_code_core::services::session_messages;
    // The inbox poll has a 250 ms deadline including the SQLite checkpoint.
    // Keep this fixture's database off a busy build disk where available.
    let root = tempfile::tempdir_in("/dev/shm")
        .or_else(|_| tempfile::tempdir())
        .unwrap();
    let f = Fixture::with_root(128_000, root).await;
    f.append("user", "Start the task");
    f.append("assistant", "Working on it");
    f.append("user", "Continue");
    let sender = f.store.fork(&f.id).unwrap();
    let marker = "UNIQUE_PEER_DELIVERY";
    session_messages::send(
        &session_messages::context(&f.paths.data_dir).unwrap(),
        &sender,
        &f.id,
        marker,
    )
    .unwrap();
    f.main_turn(None).await;
    assert_eq!(
        json!(f.store.history(&f.id).unwrap())
            .to_string()
            .matches(marker)
            .count(),
        1
    );
    f.compact().await.unwrap();
    {
        let calls = f.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for body in calls.iter() {
            assert_eq!(body.to_string().matches(marker).count(), 1);
            assert!(body["tools"].is_array());
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn reasoning_enabled_claude_reuses_the_prefix_on_both_transports() {
    for (builtin, api, model) in [
        ("anthropic", "anthropic-messages", "claude-sonnet-4-5"),
        (
            "amazon-bedrock",
            "bedrock-converse-stream",
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
        ),
    ] {
        let mut f = Fixture::new(128_000).await;
        f.provider.builtin_id = Some(builtin.into());
        f.provider.models[0].api = Some(api.into());
        f.provider.models[0].id = model.into();
        f.provider.models[0].effort = Some("medium".into());
        f.append("user", "Original question");
        f.main_turn(None).await;
        f.compact().await.unwrap();
        {
            let calls = f.requests.lock().unwrap();
            assert_eq!(calls.len(), 2);
            let mut body = calls[1].clone();
            let messages = body["messages"].as_array_mut().unwrap();
            assert!(messages
                .pop()
                .unwrap()
                .to_string()
                .contains("Keep pending work"));
            assert!(messages.pop().unwrap().to_string().contains("last answer"));
            assert_eq!(body, calls[0], "{api}");
        }
        f.finish().await;
    }
}
