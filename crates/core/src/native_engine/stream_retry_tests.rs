use super::*;
use crate::native_engine::{
    anthropic_client::AnthropicClient, openai_client::OpenAIClient,
    provider_manager::ResolvedProvider, request_identity::RequestIdentity,
    request_identity::TurnIdentity,
};
use axum::{body::Body, http::HeaderMap, routing::post, Json, Router};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[path = "../../tests/support/manual_clock.rs"]
mod manual_clock;

const ANTHROPIC_EMPTY: &str = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"empty\",\"content\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
const OPENAI_EMPTY: &str = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n";

type Captured = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

async fn fixture(
    responses: Vec<(String, bool)>,
) -> (ResolvedProvider, Captured, tokio::task::JoinHandle<()>) {
    let requests: Captured = Default::default();
    let captured = requests.clone();
    let app = Router::new().fallback(post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let mut requests = captured.lock().unwrap();
        let (text, reset) = responses[requests.len().min(responses.len() - 1)].clone();
        requests.push((headers, body));
        async move {
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(text);
                if reset {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    yield Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "fixture reset"));
                }
            };
            ([("content-type", "text/event-stream")], Body::from_stream(stream))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":base,"api_key":"fixture-only","api_format":"openai","enabled":true,"models":[]})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_web_search":false})).unwrap(),
    };
    (provider, requests, server)
}

async fn client(
    provider: &ResolvedProvider,
    kind: usize,
) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
    match kind {
        0 => OpenAIClient::new()
            .send_message_stream(provider, vec![], None, vec![], 128)
            .await
            .unwrap(),
        1 => AnthropicClient::new()
            .send_message_stream(provider, vec![], None, vec![], 128)
            .await
            .unwrap(),
        _ => AnthropicClient::new()
            .send_message_stream_with_thinking(provider, vec![], None, vec![], 128, false)
            .await
            .unwrap(),
    }
}

async fn collect(
    mut stream: Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
) -> (String, Option<String>) {
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => text.push_str(&chunk),
            Err(error) => return (text, Some(format!("{error:#}"))),
        }
    }
    (text, None)
}

#[tokio::test]
async fn empty_disconnect_recovers_without_duplicate_events_or_changed_identity() {
    for kind in 0..3 {
        for reset in [false, true] {
            let prelude = if kind == 0 {
                OPENAI_EMPTY
            } else {
                ANTHROPIC_EMPTY
            };
            let completed = if kind == 0 {
                "data: {\"choices\":[{\"delta\":{\"content\":\"Recovered 中文\"},\"finish_reason\":\"stop\"}]}\n\n"
            } else {
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Recovered 中文\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n"
            };
            let successful = format!("{prelude}{completed}");
            let (provider, requests, server) =
                fixture(vec![(prelude.into(), reset), (successful.clone(), false)]).await;
            let stream = TurnIdentity::new(Some("resumed-session".into()), None)
                .scope(client(&provider, kind))
                .await;
            let (text, error) = collect(stream).await;
            assert!(error.is_none(), "{error:?}");
            assert_eq!(text, successful);
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].1, requests[1].1);
            for (index, (headers, _)) in requests.iter().enumerate() {
                // This provider is not OpenCode, so no session ID is disclosed.
                assert!(!headers.contains_key("x-opencode-session"));
                assert!(!headers.contains_key("session-id"));
                assert_eq!(headers["x-request-id"], requests[0].0["x-request-id"]);
                assert_eq!(headers["x-dcl-turn-id"], requests[0].0["x-dcl-turn-id"]);
                assert_eq!(headers["x-dcl-retry-count"], index.to_string());
            }
            server.abort();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn empty_disconnect_is_bounded_and_can_be_cancelled_during_backoff() {
    use crate::{native_engine::tool_loop::EngineEvent, performance::retry_events};

    manual_clock::run(async {
        let (provider, requests, server) = fixture(vec![(ANTHROPIC_EMPTY.into(), false)]).await;
        let (tx, mut events) = tokio::sync::mpsc::channel(32);
        let pending = retry_events::scope(tx, async { collect(client(&provider, 2).await).await });
        tokio::pin!(pending);
        let started = tokio::time::Instant::now();
        for attempt in 1..8 {
            let event = loop {
                tokio::select! {
                    biased;
                    result = &mut pending => panic!("stream ended before retry {attempt}: {result:?}"),
                    event = events.recv() => match event.expect("retry events remain open") {
                        EngineEvent::RequestAttempt(event) if event["outcome"] == "retry_wait" => break event,
                        _ => {},
                    },
                }
            };
            let delay = Duration::from_secs(2 << (attempt - 1));
            assert_eq!(event["attempts"], attempt);
            assert_eq!(event["waitMs"], delay.as_millis() as u64);
            assert_eq!(requests.lock().unwrap().len(), attempt as usize);
            tokio::time::advance(delay - Duration::from_millis(1)).await;
            assert!(futures::poll!(&mut pending).is_pending());
            assert_eq!(requests.lock().unwrap().len(), attempt as usize);
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        let (text, error) = pending.await;
        assert_eq!(started.elapsed(), Duration::from_secs(254));
        assert_eq!(requests.lock().unwrap().len(), 8);
        assert_eq!(text, ANTHROPIC_EMPTY);
        assert!(error.unwrap().contains("attempts=8"));
        server.abort();

        let (provider, requests, server) = fixture(vec![(ANTHROPIC_EMPTY.into(), false)]).await;
        let (tx, mut events) = tokio::sync::mpsc::channel(32);
        let mut stream = retry_events::scope(tx, client(&provider, 2)).await;
        let started = tokio::time::Instant::now();
        {
            let next = stream.next();
            tokio::pin!(next);
            loop {
                tokio::select! {
                    biased;
                    result = &mut next => panic!("empty stream exposed output before retry: {result:?}"),
                    event = events.recv() => {
                        if let EngineEvent::RequestAttempt(event) = event.expect("retry events remain open") {
                            if event["outcome"] == "retry_wait" {
                                assert_eq!(event["waitMs"], 2000);
                                break;
                            }
                        }
                    },
                }
            }
            assert_eq!(started.elapsed(), Duration::ZERO);
            tokio::time::advance(Duration::from_millis(50)).await;
            assert!(futures::poll!(&mut next).is_pending());
        }
        drop(stream);
        tokio::time::advance(Duration::from_millis(2100)).await;
        assert_eq!(requests.lock().unwrap().len(), 1);
        server.abort();
    })
    .await;
}

#[tokio::test]
async fn content_tools_completion_errors_and_unknown_data_are_never_replayed() {
    for suffix in [
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"Write\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"partial\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
        "data: {\"type\":\"error\",\"error\":{\"message\":\"fixture rejected\"}}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"partial\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n",
        "data: {\"unknown\":true}\n\n",
        "data: {\"choices\":[],\"provider_content\":{\"content\":[{\"type\":\"toolCall\"}]}}\n\n",
        "data: {\"unfinished\":",
    ] {
        let source = format!("{ANTHROPIC_EMPTY}{suffix}");
        let (provider, requests, server) = fixture(vec![(source.clone(), false)]).await;
        let (text, error) = collect(client(&provider, 2).await).await;
        assert_eq!(text, source);
        assert!(error.is_none());
        assert_eq!(requests.lock().unwrap().len(), 1, "{suffix}");
        server.abort();
    }
    let source = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let (provider, requests, server) = fixture(vec![(source.into(), true)]).await;
    let (text, error) = collect(client(&provider, 0).await).await;
    assert_eq!(text, source);
    assert!(error.is_some());
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.abort();
}

const CHAT_HEADERS: &[u8] =
    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
const CHAT_TEXT: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n";
const CHAT_DONE: &str = "data: [DONE]\n\n";
const CHAT_TIMEOUT_EVENT: &str = "data: {\"error\":{\"type\":\"timeout\",\"message\":\"request timed out for https://user:fixture-secret@example.test/v1?api_key=fixture-secret\"}}\n\n";

/// Raw loopback response. `stall` keeps the body open until the client's idle
/// read deadline fires; otherwise the connection closes cleanly.
async fn scripted_body(
    prefix: Vec<u8>,
    stall: bool,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let total = count.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            total.fetch_add(1, Ordering::SeqCst);
            let prefix = prefix.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                if socket.write_all(&prefix).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                if stall {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                } else {
                    let _ = socket.shutdown().await;
                }
            });
        }
    });
    (base, count, server)
}

fn chat_opener(
    base: String,
    client: reqwest::Client,
) -> impl FnMut(&RequestIdentity) -> OpenFuture + Send + 'static {
    move |identity: &RequestIdentity| {
        let client = client.clone();
        let url = format!("{base}/chat/completions");
        let headers = identity.headers();
        Box::pin(async move {
            Ok(client
                .post(url)
                .headers(headers)
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await?)
        })
    }
}

fn idle_client() -> reqwest::Client {
    super::super::model_http::streaming_client_with_idle(
        Duration::from_millis(250),
        super::super::model_http::Redirect::None,
    )
    .unwrap()
}

/// One body case: open the response, read until it fails, assert one POST.
async fn body_failure(prefix: Vec<u8>, stall: bool) -> (String, anyhow::Error) {
    let (base, count, server) = scripted_body(prefix, stall).await;
    let (mut stream, _) = attempts_with_budget(
        RequestIdentity::new(None, false, "main"),
        chat_opener(base, idle_client()),
        super::super::provider_runtime::chat_decoder,
        AttemptBudget::new(2),
    )
    .await
    .expect("the body cases open a response");
    let mut text = String::new();
    let mut error = None;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => text.push_str(&chunk),
            Err(failure) => {
                error = Some(failure);
                break;
            }
        }
    }
    server.abort();
    assert_eq!(count.load(Ordering::SeqCst), 1, "one physical POST");
    (text, error.expect("the body stops before completion"))
}

#[tokio::test]
async fn typed_stream_body_timeout_survives_the_retry_layer() {
    use crate::performance::{find_failure, ErrorClass, RequestStage};

    // Text, then a stalled body: the typed body-timeout fact survives.
    let mut prefix = CHAT_HEADERS.to_vec();
    prefix.extend_from_slice(CHAT_TEXT.as_bytes());
    let (text, error) = body_failure(prefix, true).await;
    assert_eq!(text, CHAT_TEXT);
    let failure = find_failure(&error).expect("structured failure");
    assert!(failure.is_stream_body_timeout(), "{}", failure.diagnostic());
    assert_eq!(failure.stage(), RequestStage::Stream);
    assert_eq!(failure.class(), ErrorClass::Transient);
    assert_eq!(failure.attempts(), Some(1));
    assert_eq!(failure.status(), Some(200));
    assert_eq!(failure.terminal_marker_observed(), Some(false));

    // Text and the raw terminal marker, then a stalled body: both facts.
    let mut prefix = CHAT_HEADERS.to_vec();
    prefix.extend_from_slice(CHAT_TEXT.as_bytes());
    prefix.extend_from_slice(CHAT_DONE.as_bytes());
    let (_, error) = body_failure(prefix, true).await;
    let failure = find_failure(&error).unwrap();
    assert!(failure.is_stream_body_timeout(), "{}", failure.diagnostic());
    assert_eq!(failure.terminal_marker_observed(), Some(true));

    // Early EOF after content: not a timeout and no terminal marker.
    let mut prefix = CHAT_HEADERS.to_vec();
    prefix.extend_from_slice(CHAT_TEXT.as_bytes());
    let (_, error) = body_failure(prefix, false).await;
    let failure = find_failure(&error).unwrap();
    assert!(!failure.is_stream_body_timeout());
    assert_ne!(failure.terminal_marker_observed(), Some(true));

    // A provider timeout-shaped SSE error stays remote data, reduced and
    // sanitized, and never becomes the typed transport fact.
    let mut prefix = CHAT_HEADERS.to_vec();
    prefix.extend_from_slice(CHAT_TIMEOUT_EVENT.as_bytes());
    let (_, error) = body_failure(prefix, false).await;
    let failure = find_failure(&error).unwrap();
    assert!(!failure.is_stream_body_timeout());
    assert_eq!(failure.terminal_marker_observed(), Some(false));
    let json = failure.as_json();
    let keys: std::collections::BTreeSet<_> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "stage",
            "class",
            "http",
            "attempts",
            "retries",
            "contentStarted",
            "cause",
            "serverRequestId"
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(json["stage"], "stream");
    assert_eq!(json["http"], 200);
    assert_eq!(json["attempts"], 1);
    assert_eq!(json["contentStarted"], false);
    assert!(
        !failure.cause().contains("fixture-secret") && !failure.cause().contains("user:"),
        "{}",
        failure.cause()
    );

    // A timeout before headers is a send failure and carries no typed fact.
    let (base, count, server) = scripted_body(Vec::new(), true).await;
    let error = match attempts_with_budget(
        RequestIdentity::new(None, false, "main"),
        chat_opener(base, idle_client()),
        super::super::provider_runtime::chat_decoder,
        AttemptBudget::new(1),
    )
    .await
    {
        Ok(_) => panic!("a header wait times out"),
        Err(error) => error,
    };
    server.abort();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let failure = find_failure(&error).unwrap();
    assert!(!failure.is_stream_body_timeout());
    assert_eq!(failure.stage(), RequestStage::Send);
    assert_eq!(failure.attempts(), Some(1));
    assert_eq!(failure.status(), None);
}
