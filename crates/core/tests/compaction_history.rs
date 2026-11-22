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
        deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(root.path(), "model", "fixture").unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let app = Router::new().fallback(post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let compact = headers["x-dcl-request-kind"] == "compaction";
            let stream = body["stream"] == true;
            let text = if compact { "Pending work is recorded." } else { "last answer" };
            seen.lock().unwrap().push(body);
            async move {
                let usage = json!({"prompt_tokens":100,"completion_tokens":3,"total_tokens":103,"prompt_tokens_details":{"cached_tokens":80}});
                if stream {
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

    async fn main_turn(&self) {
        let request: ChatRequest = serde_json::from_value(json!({"conversation_id":self.id,
            "messages":self.store.model_history(&self.id,&self.provider.api_format).unwrap(),
            "system_prompt":"Custom session instructions","model":"model","provider_id":"fixture","workspace_path":self.root.path()})).unwrap();
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
        self.append("assistant", "last answer");
    }

    async fn compact(&self) -> anyhow::Result<compaction::CompactionResult> {
        compaction::compact(
            &self.engine(),
            &self.store.db,
            &self.id,
            "fixture",
            "model",
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
    f.main_turn().await;
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
