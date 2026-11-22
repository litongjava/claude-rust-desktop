//! A single-use copy of the last wire request of one main turn. Nothing from
//! the follow-up passes through the tool loop, turn accounting or transcript.
use super::{
    anthropic_client::AnthropicClient, openai_client::OpenAIClient,
    provider_manager::ResolvedProvider, provider_runtime, request_identity::TurnIdentity,
    request_options::RequestOptions, request_trace::RequestTrace,
};
use crate::session_files::suggestion::SuggestionGuard;
use anyhow::{Context, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const TTL: Duration = Duration::from_secs(300);
const INSTRUCTION: &str = "Write the next message this user is likely to type, using their language and style. Return only 2–12 words. Leave the reply empty if their next step is unclear. Omit evaluations, questions, assistant narration, and ideas the user hasn't raised.";

// The transport records the final JSON after protocol-specific changes. Only
// the completion of that very request can make it eligible for a follow-up.
#[derive(Clone, Default)]
pub struct SuggestionCapture(Arc<Mutex<CaptureState>>);

#[derive(Default)]
struct CaptureState {
    body: Option<(String, Value)>,
    builtin_vision: Option<bool>,
    automatic_cache: bool,
    completed: Option<CompletedRequest>,
}

struct CompletedRequest {
    provider: ResolvedProvider,
    options: RequestOptions,
    finished: Instant,
}

impl SuggestionCapture {
    pub(super) fn record(
        &self,
        protocol: &str,
        body: &Value,
        builtin_vision: Option<bool>,
        url: &str,
        model: &str,
    ) {
        *self.0.lock().unwrap() = CaptureState {
            body: Some((protocol.to_owned(), body.clone())),
            automatic_cache: automatic_cache(url, model),
            builtin_vision,
            completed: None,
        };
    }

    pub(super) fn finish(
        &self,
        snapshot: &Value,
        provider: &ResolvedProvider,
        options: &RequestOptions,
    ) {
        let mut capture = self.0.lock().unwrap();
        let cache_read = snapshot["cacheReadTokens"].as_u64().unwrap_or_default() > 0;
        if snapshot["status"] == "completed" && (cache_read || capture.automatic_cache) {
            capture.completed = Some(CompletedRequest {
                provider: provider.clone(),
                options: options.clone().with_purpose("suggestion"),
                finished: Instant::now(),
            });
        } else {
            *capture = CaptureState::default();
        }
    }

    pub async fn generate(self, guard: Arc<SuggestionGuard>) -> Result<Option<String>> {
        let capture = std::mem::take(&mut *self.0.lock().unwrap());
        let (
            Some((api, mut body)),
            Some(CompletedRequest {
                provider,
                mut options,
                finished,
            }),
        ) = (capture.body, capture.completed)
        else {
            return Ok(None);
        };
        if finished.elapsed() > TTL || !guard.is_current()? {
            return Ok(None);
        }
        options.suggestion_guard = Some((guard.clone(), finished + TTL));
        append_instruction(&api, &mut body)?;
        let identity = TurnIdentity::new(options.session_id.clone(), None);
        let operation = async {
            let budget = crate::performance::AttemptBudget::single_attempt();
            let direct_anthropic = capture.builtin_vision.is_none() && api == "anthropic-messages";
            let (mut stream, _) = if let Some(vision) = capture.builtin_vision {
                provider_runtime::stream_body(
                    &provider,
                    body,
                    "user",
                    vision,
                    0,
                    None,
                    &options,
                    super::model_http::single_post_client()?,
                    budget,
                )
                .await?
            } else if direct_anthropic {
                AnthropicClient::new()
                    .with_request_options(options)
                    .stream_body(&provider, body, 0, None)
                    .await?
            } else {
                OpenAIClient::new()
                    .with_request_options(options)
                    .stream_body(&provider, body, 0, None, budget)
                    .await?
            };
            let mut response = String::new();
            while let Some(chunk) = stream.next().await {
                response.push_str(&chunk?);
                if response.len() > 256 * 1024 {
                    return Ok(None);
                }
            }
            response.push_str("\n\n");
            let mut trace = RequestTrace::new(json!({}));
            for payload in crate::streaming::sse_parser::consume_sse_payloads(&response).payloads {
                if payload == "[DONE]" {
                    trace.mark_completed();
                    continue;
                }
                let event = serde_json::from_str(&payload)?;
                if direct_anthropic {
                    trace.observe_anthropic(&event);
                } else {
                    trace.observe_openai(&event);
                }
            }
            let snapshot = trace.finish("completed", None);
            let blocks = snapshot["responseBlocks"]
                .as_array()
                .context("Missing response blocks")?;
            if snapshot["status"] != "completed"
                || blocks.iter().any(|b| b["type"] == "tool_call")
                || matches!(
                    snapshot["finishReason"].as_str(),
                    Some("tool_calls" | "tool_use")
                )
            {
                return Ok(None);
            }
            Ok(Some(
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect(),
            ))
        };
        let invalidated = async {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if finished.elapsed() > TTL || !guard.is_current()? {
                    return Ok(None);
                }
            }
        };
        tokio::select! {
            result = invalidated => result,
            result = identity.scope(tokio::time::timeout(Duration::from_secs(15), operation)) => result?,
        }
    }
}

fn append_instruction(api: &str, body: &mut Value) -> Result<()> {
    let (key, message) = match api {
        "openai-completions" | "anthropic-messages" => {
            ("messages", json!({"role":"user","content":INSTRUCTION}))
        }
        "openai-responses" => (
            "input",
            json!({"role":"user","content":[{"type":"input_text","text":INSTRUCTION}]}),
        ),
        "google-generative-ai" => (
            "contents",
            json!({"role":"user","parts":[{"text":INSTRUCTION}]}),
        ),
        "bedrock-converse-stream" => (
            "messages",
            json!({"role":"user","content":[{"text":INSTRUCTION}]}),
        ),
        _ => anyhow::bail!("Unknown suggestion protocol {api}"),
    };
    body[key]
        .as_array_mut()
        .context("Missing wire message list")?
        .push(message);
    Ok(())
}

// Groq documents automatic prompt caching only for these model families.
// Every other Groq model must show a cache read like any custom endpoint.
const GROQ_CACHED_MODEL_PREFIXES: &[&str] = &[
    "openai/gpt-oss-20b",
    "openai/gpt-oss-120b",
    "openai/gpt-oss-safeguard-20b",
];

// These services document automatic prefix caching. Match the final model URL,
// including catalog defaults and regional endpoints. A protocol ID, a cache
// price, or support for explicit cache markers says nothing about a proxy.
fn automatic_cache(url: &str, model: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_end_matches('.');
    if host == "api.groq.com" {
        return GROQ_CACHED_MODEL_PREFIXES
            .iter()
            .any(|prefix| model.starts_with(prefix));
    }
    matches!(
        host,
        "api.deepseek.com"
            | "api.openai.com"
            | "chatgpt.com"
            | "api.x.ai"
            | "generativelanguage.googleapis.com"
            | "aiplatform.googleapis.com"
    ) || host.ends_with("-aiplatform.googleapis.com")
        || [
            ".openai.azure.com",
            ".openai.azure.us",
            ".openai.azure.cn",
            ".services.ai.azure.com",
        ]
        .iter()
        .any(|suffix| host.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_engine::provider_manager::Provider;

    #[test]
    fn automatic_cache_depends_on_the_service_host_and_model() {
        for host in [
            "api.deepseek.com",
            "API.OPENAI.COM",
            "chatgpt.com",
            "west.openai.azure.com",
            "svc.openai.azure.us",
            "svc.openai.azure.cn",
            "review.services.ai.azure.com",
            "api.x.ai",
            "generativelanguage.googleapis.com",
            "aiplatform.googleapis.com",
            "us-central1-aiplatform.googleapis.com",
        ] {
            assert!(
                automatic_cache(&format!("http://{host}:1234/v1"), "any-model"),
                "{host}"
            );
        }
        for model in [
            "openai/gpt-oss-120b",
            "openai/gpt-oss-20b",
            "openai/gpt-oss-safeguard-20b",
        ] {
            assert!(
                automatic_cache("https://api.groq.com/openai/v1/chat/completions", model),
                "{model}"
            );
        }
        for model in [
            "llama-3.1-8b-instant",
            "llama-3.3-70b-versatile",
            "qwen/qwen3.6-27b",
        ] {
            assert!(
                !automatic_cache("https://api.groq.com/openai/v1/chat/completions", model),
                "{model}"
            );
        }
        for (url, model) in [
            ("http://127.0.0.1/openai", "any-model"),
            ("https://api.openai.com.evil.example", "any-model"),
            ("https://api.groq.com.evil.example", "openai/gpt-oss-120b"),
            ("https://services.ai.azure.com.evil.example", "gpt-4.1"),
            ("https://api.groq.com@proxy.example", "openai/gpt-oss-120b"),
            ("https://notopenai.azure.com", "any-model"),
            ("https://api.x.ai.example", "any-model"),
            (
                "https://us-east1-aiplatform.googleapis.com.example",
                "any-model",
            ),
            ("invalid", "any-model"),
        ] {
            assert!(!automatic_cache(url, model), "{url}");
        }
    }

    #[tokio::test]
    async fn expired_main_request_and_cold_custom_provider_send_nothing() {
        let endpoint = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config: Provider = serde_json::from_value(json!({
            "id":"mock","name":"fixture","base_url":format!("http://{}",endpoint.local_addr().unwrap()),
            "api_key":"mock","api_format":"openai","enabled":true,
            "models":[{"id":"mock","name":"fixture","enabled":true,"supports_web_search":false}]
        })).unwrap();
        let provider = ResolvedProvider {
            model: config.models[0].clone(),
            provider: config,
        };
        use crate::{
            db::turn_repo::{TurnStatus, TurnTimer},
            runtime::{RuntimePaths, SessionStore},
            session_files::suggestion::SuggestionTurn,
        };
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let session = store.create(root.path(), "mock", "mock").unwrap();
        let mut turn = store.db.start_turn_timer(&session).unwrap();
        let origin = SuggestionTurn::begin(
            store.db.clone(),
            &session,
            &turn.record().turn_id,
            "mock",
            "mock",
        )
        .unwrap()
        .unwrap();
        let assistant = turn.ensure_segment().unwrap();
        turn.finish(TurnStatus::Completed).unwrap();
        let guard = Arc::new(origin.complete(&assistant).unwrap().unwrap());
        assert!(guard.is_current().unwrap());
        for (cached, expired) in [(0, false), (32, true)] {
            let capture = SuggestionCapture::default();
            capture.record(
                "openai-completions",
                &json!({"messages":[]}),
                None,
                &provider.provider.base_url,
                &provider.model.id,
            );
            capture.finish(
                &json!({"status":"completed","cacheReadTokens":cached}),
                &provider,
                &RequestOptions::for_session(Some("session")),
            );
            if expired {
                capture
                    .0
                    .lock()
                    .unwrap()
                    .completed
                    .as_mut()
                    .unwrap()
                    .finished -= TTL + Duration::from_secs(1);
            }
            assert!(capture.generate(guard.clone()).await.unwrap().is_none());
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), endpoint.accept())
                .await
                .is_err()
        );
    }

    async fn groq_fixture() -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let events = concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"run the checks\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{events}",
                    events.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (url, requests, task)
    }

    #[tokio::test]
    async fn groq_llama_cold_turn_sends_no_suggestion() {
        use crate::{
            db::turn_repo::{TurnStatus, TurnTimer},
            runtime::{RuntimePaths, SessionStore},
            session_files::suggestion::SuggestionTurn,
        };
        for (model, expected) in [
            ("llama-3.1-8b-instant", None),
            ("openai/gpt-oss-120b", Some("run the checks")),
        ] {
            let (base_url, requests, server) = groq_fixture().await;
            let config: Provider = serde_json::from_value(json!({
                "id":"groq-fixture","name":"fixture","base_url":base_url,
                "api_key":"mock","api_format":"openai","enabled":true,
                "models":[{"id":model,"name":"fixture","enabled":true,"supports_web_search":false}]
            }))
            .unwrap();
            let provider = ResolvedProvider {
                model: config.models[0].clone(),
                provider: config,
            };
            let root = tempfile::tempdir().unwrap();
            let paths = RuntimePaths::new(root.path().join("data")).unwrap();
            let store = SessionStore::new(paths.open_database().unwrap());
            let session = store.create(root.path(), model, "groq-fixture").unwrap();
            let mut turn = store.db.start_turn_timer(&session).unwrap();
            let origin = SuggestionTurn::begin(
                store.db.clone(),
                &session,
                &turn.record().turn_id,
                model,
                "groq-fixture",
            )
            .unwrap()
            .unwrap();
            let assistant = turn.ensure_segment().unwrap();
            turn.finish(TurnStatus::Completed).unwrap();
            let guard = Arc::new(origin.complete(&assistant).unwrap().unwrap());

            let capture = SuggestionCapture::default();
            capture.record(
                "openai-completions",
                &json!({"messages":[]}),
                None,
                "https://api.groq.com/openai/v1/chat/completions",
                model,
            );
            capture.finish(
                &json!({"status":"completed","cacheReadTokens":0}),
                &provider,
                &RequestOptions::for_session(Some("session")),
            );
            assert_eq!(
                capture.generate(guard).await.unwrap().as_deref(),
                expected,
                "{model}"
            );
            assert_eq!(
                requests.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(expected.is_some()),
                "{model}"
            );
            server.abort();
        }
    }

    #[test]
    fn append_only_one_user_message_in_each_wire_format() {
        for (api, key) in [
            ("openai-completions", "messages"),
            ("openai-responses", "input"),
            ("anthropic-messages", "messages"),
            ("google-generative-ai", "contents"),
            ("bedrock-converse-stream", "messages"),
        ] {
            let original = json!({key:[{"role":"user","content":"first"}],"tools":[{"name":"z"},{"name":"a"}]});
            let mut body = original.clone();
            append_instruction(api, &mut body).unwrap();
            let added = body[key].as_array_mut().unwrap().pop().unwrap();
            assert_eq!(added["role"], "user");
            assert!(added.to_string().contains(INSTRUCTION));
            assert!(!added.to_string().contains("cache_control"));
            assert_eq!(body, original);
        }
    }
}
