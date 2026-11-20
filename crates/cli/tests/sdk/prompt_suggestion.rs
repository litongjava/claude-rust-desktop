use super::*;
use axum::{body::Bytes, http::HeaderMap};
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    session_files::suggestion::SavedSuggestion,
};

pub(super) struct Endpoint {
    api: &'static str,
    url: String,
    pub(super) requests: Requests,
    task: tokio::task::JoinHandle<()>,
}

type Requests = Arc<Mutex<Vec<(bool, String)>>>;

async fn reply(
    State((api, captured)): State<(&'static str, Requests)>,
    headers: HeaderMap,
    bytes: Bytes,
) -> impl IntoResponse {
    let suggestion = headers
        .get("x-dcl-request-kind")
        .is_some_and(|v| v == "suggestion");
    let raw = String::from_utf8(bytes.to_vec()).unwrap();
    let mut requests = captured.lock().unwrap();
    let next_turn = requests.iter().any(|(s, _)| *s);
    let cache_read = if next_turn && !suggestion {
        let original: Value = serde_json::from_str(&requests[0].1).unwrap();
        let current: Value = serde_json::from_str(&raw).unwrap();
        let key = if api == "responses" {
            "input"
        } else {
            "messages"
        };
        let first = original[key].as_array().unwrap();
        // Anthropic marks the last user block anew on every main request.
        // The system prefix and tool definitions still match.
        let prefix_matches = if api == "anthropic" {
            !original["system"].is_null()
                && current["system"] == original["system"]
                && current["tools"] == original["tools"]
        } else {
            current[key].as_array().unwrap().starts_with(first)
        };
        if prefix_matches {
            96
        } else {
            0
        }
    } else {
        48
    };
    let load_tool =
        api == "responses" && requests.is_empty() && raw.contains("LOAD_LISTDIR_FIXTURE");
    requests.push((suggestion, raw));
    let text = if suggestion {
        "run the checks"
    } else {
        "main reply"
    };
    let tool = suggestion && next_turn;
    let events = match api {
        "chat" => {
            let delta = if tool {
                json!({"content":text,"tool_calls":[{"index":0,"id":"ignored","type":"function","function":{"name":"Write","arguments":"{}"}}]})
            } else {
                json!({"content":text})
            };
            let event = json!({
                "choices":[{"index":0,"delta":delta,"finish_reason":if tool {"tool_calls"} else {"stop"}}],
                "usage":{"prompt_tokens":128,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":cache_read}}
            });
            format!("data: {event}\n\ndata: [DONE]\n\n")
        }
        "anthropic" => {
            let block = if tool {
                json!({"type":"tool_use","id":"ignored","name":"Write","input":{}})
            } else {
                json!({"type":"text","text":""})
            };
            let mut frames = vec![
                json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":32,"cache_read_input_tokens":cache_read}}}),
                json!({"type":"content_block_start","index":0,"content_block":block}),
            ];
            if !tool {
                frames.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}));
            }
            frames.extend([
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}}}),
                json!({"type":"message_stop"})
            ]);
            frames
                .into_iter()
                .map(|v| format!("data: {v}\n\n"))
                .collect()
        }
        _ if load_tool => responses_tool_call("load", "ToolLoad", json!({"names":["ListDir"]})),
        _ if tool => responses_tool_call("ignored", "Write", json!({})),
        _ => responses_text(text),
    };
    ([("content-type", "text/event-stream")], events)
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Endpoint {
    pub(super) async fn start(api: &'static str) -> Self {
        let requests = Arc::new(Mutex::new(Vec::<(bool, String)>::new()));
        let captured = requests.clone();
        let path = match api {
            "chat" => "/v1/chat/completions",
            "anthropic" => "/v1/messages",
            _ => "/responses",
        };
        let app = Router::new()
            .route(path, post(reply))
            .with_state((api, captured));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            api,
            url,
            requests,
            task,
        }
    }

    pub(super) fn client(&self, root: &Path, extra: &[&str]) -> Client {
        if self.api == "responses" {
            write_responses_provider(root, &self.url);
            let mut args = vec![
                "--provider",
                "responses-fixture",
                "--model",
                "fixture-model",
            ];
            args.extend_from_slice(extra);
            Client::start_configured(root, &args)
        } else {
            Client::launch(
                root,
                Some((
                    &self.url,
                    if self.api == "chat" {
                        "openai"
                    } else {
                        "anthropic"
                    },
                )),
                extra,
            )
        }
    }

    fn verify_pair(&self, first: usize) {
        let requests = self.requests.lock().unwrap();
        let (main_kind, main) = &requests[first];
        let (suggestion_kind, suggestion) = &requests[first + 1];
        assert!(!main_kind && *suggestion_kind);
        let body: Value = serde_json::from_str(suggestion).unwrap();
        let key = if self.api == "responses" {
            assert!(body["prompt_cache_key"].is_string());
            "input"
        } else {
            "messages"
        };
        assert!(!body["tools"].as_array().unwrap().is_empty());
        let messages = body[key].as_array().unwrap();
        let appended = messages.last().unwrap();
        assert_eq!(appended["role"], "user");
        assert!(appended.to_string().contains("2–12"));
        assert!(!appended.to_string().contains("cache_control"));
        // Remove only the newly serialized array element from the raw HTTP
        // bytes. All original punctuation, ordering and escaping must survive.
        let suffix = format!(",{appended}");
        assert_eq!(suggestion.matches(&suffix).count(), 1);
        assert_eq!(
            suggestion.replacen(&suffix, "", 1).as_bytes(),
            main.as_bytes()
        );
    }
}

async fn initialize(client: &mut Client, enabled: bool) {
    client
        .send(initialize_request(
            "init",
            json!({"promptSuggestions":enabled}),
        ))
        .await;
    assert_eq!(
        client.frame("control_response").await["response"]["subtype"],
        "success"
    );
}

#[tokio::test]
async fn wire_prefix_history_tool_discard_and_zero_request_resume() {
    for api in ["chat", "anthropic", "responses"] {
        let server = Endpoint::start(api).await;
        let root = tempfile::tempdir().unwrap();
        let mut client = server.client(root.path(), &[]);
        initialize(&mut client, true).await;
        client.send(user("check the build")).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false, "{api}: {result}");
        let session = result["session_id"].as_str().unwrap();
        assert_eq!(
            client.frame("prompt_suggestion").await["suggestion"],
            "run the checks"
        );
        assert!(client.finish().await.0.success());
        server.verify_pair(0);

        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let saved = SavedSuggestion::load(&store.db, session).unwrap().unwrap();
        assert_eq!(saved.position.history_len, 2);
        assert_eq!(
            store.history(session).unwrap(),
            vec![
                json!({"role":"user","content":"check the build"}),
                json!({"role":"assistant","content":"main reply"})
            ]
        );

        let resume = ["--resume", session];
        let mut client = server.client(root.path(), &resume);
        initialize(&mut client, true).await;
        assert_eq!(
            client.frame("prompt_suggestion").await["suggestion"],
            saved.text
        );
        let (status, _, remaining) = client.finish_with_transcript().await;
        assert!(status.success() && !remaining.contains("prompt_suggestion"));
        assert_eq!(server.requests.lock().unwrap().len(), 2);

        let mut elsewhere = server.client(root.path(), &resume);
        initialize(&mut elsewhere, true).await;
        elsewhere.frame("prompt_suggestion").await;
        elsewhere.send(user("another turn")).await;
        assert_eq!(elsewhere.result().await["is_error"], false);
        let (status, _, remaining) = elsewhere.finish_with_transcript().await;
        assert!(
            status.success()
                && !remaining.contains("prompt_suggestion")
                && !remaining.contains("tool_use")
        );
        server.verify_pair(2);
        let main = server.requests.lock().unwrap()[2].1.clone();
        assert!(!main.contains("2–12") && !main.contains("run the checks"));
        assert_eq!(store.history(session).unwrap().len(), 4);
        assert!(SavedSuggestion::load(&store.db, session).unwrap().is_none());
        let mut stale = server.client(root.path(), &resume);
        initialize(&mut stale, true).await;
        let (status, _, remaining) = stale.finish_with_transcript().await;
        assert!(status.success() && !remaining.contains("prompt_suggestion"));
        assert_eq!(server.requests.lock().unwrap().len(), 4);
    }
}

#[tokio::test]
async fn disabled_setting_suppresses_sdk_resume_and_generation() {
    let endpoint = Endpoint::start("chat").await;
    let root = tempfile::tempdir().unwrap();
    let mut first = endpoint.client(root.path(), &[]);
    initialize(&mut first, true).await;
    first.send(user("check the build")).await;
    let result = first.result().await;
    let session = result["session_id"].as_str().unwrap();
    first.frame("prompt_suggestion").await;
    assert!(first.finish().await.0.success());
    std::fs::write(
        root.path().join("data/settings.json"),
        r#"{"promptSuggestionEnabled":false}"#,
    )
    .unwrap();
    let mut resumed = endpoint.client(root.path(), &["--resume", session]);
    initialize(&mut resumed, true).await;
    resumed.send(user("another turn")).await;
    let (status, results, frames) = resumed.finish_with_transcript().await;
    assert!(status.success() && results.len() == 1 && !frames.contains("prompt_suggestion"));
    assert_eq!(endpoint.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn final_tool_round_reuses_the_full_history_and_loaded_tools() {
    let endpoint = Endpoint::start("responses").await;
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store
        .create(root.path(), "fixture-model", "responses-fixture")
        .unwrap();
    let long = format!("{}original request tail", "old context ".repeat(180));
    store.append(&session, "user", &long).unwrap();
    store
        .append(&session, "assistant", "initial reply")
        .unwrap();
    for i in 0..5 {
        store
            .append(&session, "user", &format!("previous question {i}"))
            .unwrap();
        store
            .append(&session, "assistant", &format!("previous reply {i}"))
            .unwrap();
    }
    let mut client = endpoint.client(
        root.path(),
        &["--resume", &session, "--permission-mode", "bypass"],
    );
    initialize(&mut client, true).await;
    client.send(user("LOAD_LISTDIR_FIXTURE")).await;
    assert_eq!(client.result().await["is_error"], false);
    assert_eq!(
        client.frame("prompt_suggestion").await["suggestion"],
        "run the checks"
    );
    assert!(client.finish().await.0.success());
    endpoint.verify_pair(1);
    let requests = endpoint.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let initial: Value = serde_json::from_str(&requests[0].1).unwrap();
    let last: Value = serde_json::from_str(&requests[1].1).unwrap();
    assert!(!responses_tool_names(&initial).contains(&"ListDir".to_owned()));
    assert!(responses_tool_names(&last).contains(&"ListDir".to_owned()));
    assert!(requests[2].1.contains(&long));
    assert!(last["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["type"] == "function_call_output"));
}
