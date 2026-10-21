//! Local Remote Control server with simulated data, for development and tests.
//! It implements the environment, session, work and ingress endpoints used by
//! [`super::Bridge`] plus a `/mock/*` control API that plays the remote client.
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_tungstenite::tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    Message,
};

#[derive(Default)]
struct State {
    base: String,
    token: Option<String>,
    environments: HashMap<String, String>,
    deregistered: Vec<String>,
    sessions: HashMap<String, Value>,
    pending: VecDeque<(String, String)>,
    work: HashMap<String, String>,
    requests: Vec<Value>,
    events: Vec<Value>,
    clients: Vec<mpsc::UnboundedSender<Option<String>>>,
    poll_failures: VecDeque<u16>,
    on_connect: Vec<Value>,
    counter: u64,
}

impl State {
    fn next_id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}_mock_{:04}", self.counter)
    }
}

/// A running mock server; dropped servers stop accepting connections.
pub struct MockServer {
    pub url: String,
    state: Arc<Mutex<State>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockServer {
    /// `token`, when set, is required on environment and session API calls.
    /// `on_connect` frames are sent to every new ingress connection.
    pub async fn start(port: u16, token: Option<String>, on_connect: Vec<Value>) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .context("Mock bind failed")?;
        let url = format!("http://{}", listener.local_addr()?);
        let state = Arc::new(Mutex::new(State {
            base: url.clone(),
            token,
            on_connect,
            ..Default::default()
        }));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = shared.clone();
                tokio::spawn(async move {
                    if let Err(error) = connection(stream, state).await {
                        tracing::debug!("Remote Control mock connection: {error:#}");
                    }
                });
            }
        });
        Ok(Self { url, state, task })
    }

    pub fn snapshot(&self) -> Value {
        snapshot(&self.state.lock().unwrap())
    }

    /// Sends a frame to every ingress client; returns the client count.
    pub fn send(&self, frame: Value) -> usize {
        broadcast(&mut self.state.lock().unwrap(), Some(frame.to_string()))
    }

    /// Closes all ingress connections to exercise reconnect.
    pub fn drop_clients(&self) -> usize {
        broadcast(&mut self.state.lock().unwrap(), None)
    }

    pub fn fail_polls(&self, statuses: &[u16]) {
        self.state.lock().unwrap().poll_failures.extend(statuses);
    }

    pub fn events(&self) -> Vec<Value> {
        self.state.lock().unwrap().events.clone()
    }

    pub fn clients(&self) -> usize {
        let mut state = self.state.lock().unwrap();
        state.clients.retain(|client| !client.is_closed());
        state.clients.len()
    }

    /// Polls `condition` against the snapshot until it holds.
    pub async fn wait_for(
        &self,
        timeout: Duration,
        condition: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let value = self.snapshot();
            if condition(&value) {
                return Ok(value);
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "Mock condition not reached: {value}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn broadcast(state: &mut State, frame: Option<String>) -> usize {
    state
        .clients
        .retain(|client| client.send(frame.clone()).is_ok());
    let count = state.clients.len();
    if frame.is_none() {
        state.clients.clear();
    }
    count
}

fn snapshot(state: &State) -> Value {
    json!({
        "requests": state.requests,
        "events": state.events,
        "sessions": state.sessions,
        "environments": state.environments.keys().collect::<Vec<_>>(),
        "deregistered": state.deregistered,
        "clients": state.clients.iter().filter(|c| !c.is_closed()).count(),
    })
}

async fn connection(mut stream: TcpStream, state: Arc<Mutex<State>>) -> Result<()> {
    let mut head = vec![0; 16 * 1024];
    let end = loop {
        let read = stream.peek(&mut head).await?;
        anyhow::ensure!(read > 0, "Connection closed");
        if let Some(end) = head[..read].windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        anyhow::ensure!(read < head.len(), "Request head too large");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let text = String::from_utf8_lossy(&head[..end]).to_lowercase();
    if text.contains("upgrade: websocket") {
        return ingress(stream, state).await;
    }
    let mut buffer = vec![0; end];
    stream.read_exact(&mut buffer).await?;
    let head = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split_whitespace();
    let method = start.next().unwrap_or_default().to_owned();
    let target = start.next().unwrap_or_default().to_owned();
    let mut headers = HashMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_owned());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    anyhow::ensure!(length <= 8 * 1024 * 1024, "Request body too large");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let (status, response) = route(&state, &method, &target, bearer.as_deref(), &headers, body);
    let payload = response.to_string();
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    stream
        .write_all(format!("HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}", payload.len()).as_bytes())
        .await?;
    stream.shutdown().await?;
    Ok(())
}

fn route(
    state: &Arc<Mutex<State>>,
    method: &str,
    target: &str,
    bearer: Option<&str>,
    headers: &HashMap<String, String>,
    body: Value,
) -> (u16, Value) {
    let mut state = state.lock().unwrap();
    let path = target.split('?').next().unwrap_or_default().to_owned();
    if !path.starts_with("/mock/") {
        state.requests.push(json!({"method": method, "path": path, "beta": headers.get("anthropic-beta"), "body": body}));
    }
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let api_authorized = state.token.is_none() || state.token.as_deref() == bearer;
    let unauthorized = (
        401,
        json!({"error": {"type": "authentication_error", "message": "invalid token"}}),
    );
    match (method, segments.as_slice()) {
        ("GET", ["mock", "state"]) => (200, snapshot(&state)),
        ("POST", ["mock", "send"]) => (
            200,
            json!({"delivered": broadcast(&mut state, Some(body.to_string()))}),
        ),
        ("POST", ["mock", "drop"]) => (200, json!({"dropped": broadcast(&mut state, None)})),
        ("POST", ["mock", "fail"]) => {
            let status = body["status"].as_u64().unwrap_or(500) as u16;
            for _ in 0..body["count"].as_u64().unwrap_or(1) {
                state.poll_failures.push_back(status);
            }
            (200, json!({}))
        }
        ("POST", ["v1", "environments", "bridge"]) => {
            if !api_authorized {
                return unauthorized;
            }
            let id = body["environment_id"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| state.next_id("env"));
            let secret = state.next_id("secret");
            state.environments.insert(id.clone(), secret.clone());
            (
                200,
                json!({"environment_id": id, "environment_secret": secret}),
            )
        }
        ("DELETE", ["v1", "environments", "bridge", id]) => {
            if !api_authorized {
                return unauthorized;
            }
            let id = id.to_string();
            if state.environments.remove(&id).is_none() {
                return (404, json!({"error": {"message": "unknown environment"}}));
            }
            state.deregistered.push(id);
            (200, json!({}))
        }
        ("POST", ["v1", "sessions"]) => {
            if !api_authorized {
                return unauthorized;
            }
            let environment = body["environment_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if !state.environments.contains_key(&environment) {
                return (404, json!({"error": {"message": "unknown environment"}}));
            }
            let id = state.next_id("session");
            state.sessions.insert(id.clone(), json!({"title": body["title"], "environment_id": environment, "archived": false, "source": body["source"], "permission_mode": body["permission_mode"]}));
            state.pending.push_back((environment, id.clone()));
            (200, json!({"id": id}))
        }
        ("POST", ["v1", "sessions", id, "archive"]) => {
            if !api_authorized {
                return unauthorized;
            }
            match state.sessions.get_mut(*id) {
                Some(session) if session["archived"] == true => {
                    (409, json!({"error": {"message": "already archived"}}))
                }
                Some(session) => {
                    session["archived"] = json!(true);
                    (200, json!({}))
                }
                None => (404, json!({"error": {"message": "unknown session"}})),
            }
        }
        ("GET", ["v1", "environments", env, "work", "poll"]) => {
            if state.environments.get(*env).map(String::as_str) != bearer || bearer.is_none() {
                return unauthorized;
            }
            if let Some(status) = state.poll_failures.pop_front() {
                return (status, json!({"error": {"message": "simulated failure"}}));
            }
            let Some(index) = state.pending.iter().position(|(e, _)| e == env) else {
                return (200, Value::Null);
            };
            let (environment, session) = state.pending.remove(index).unwrap();
            let work = state.next_id("work");
            let token = format!("ingress_{session}");
            state.work.insert(work.clone(), token.clone());
            let secret = URL_SAFE_NO_PAD.encode(
                json!({"version": 1, "session_ingress_token": token, "api_base_url": state.base})
                    .to_string(),
            );
            (
                200,
                json!({"id": work, "type": "work", "environment_id": environment, "state": "queued", "data": {"type": "session", "id": session}, "secret": secret, "created_at": chrono::Utc::now().to_rfc3339()}),
            )
        }
        ("POST", ["v1", "environments", _env, "work", work, _action]) => {
            if state.work.get(*work).map(String::as_str) != bearer || bearer.is_none() {
                return unauthorized;
            }
            (200, json!({}))
        }
        ("POST", ["v1", "environments", env, "bridge", "reconnect"]) => {
            if !api_authorized {
                return unauthorized;
            }
            let session = body["session_id"].as_str().unwrap_or_default().to_owned();
            if !state.sessions.contains_key(&session) {
                return (404, json!({"error": {"message": "unknown session"}}));
            }
            state.pending.push_back((env.to_string(), session));
            (200, json!({}))
        }
        ("POST", ["v1", "session_ingress", "session", id, "events"]) => {
            if bearer != Some(format!("ingress_{id}").as_str()) {
                return unauthorized;
            }
            let events = body["events"].as_array().cloned().unwrap_or_default();
            state.events.extend(events);
            (200, json!({}))
        }
        _ => (
            404,
            json!({"error": {"message": format!("no mock route for {method} {path}")}}),
        ),
    }
}

async fn ingress(stream: TcpStream, state: Arc<Mutex<State>>) -> Result<()> {
    let authorized = state.clone();
    let callback = move |request: &Request,
                         response: Response|
          -> std::result::Result<Response, ErrorResponse> {
        let session = request
            .uri()
            .path()
            .strip_prefix("/v1/session_ingress/ws/")
            .unwrap_or_default();
        let bearer = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        let mut state = authorized.lock().unwrap();
        state
            .requests
            .push(json!({"method": "WS", "path": request.uri().path()}));
        if session.is_empty() || bearer != Some(format!("Bearer ingress_{session}").as_str()) {
            let mut denied = ErrorResponse::new(Some("invalid ingress token".into()));
            *denied.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
            return Err(denied);
        }
        Ok(response)
    };
    let mut socket = tokio_tungstenite::accept_hdr_async(stream, callback).await?;
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let greeting = {
        let mut state = state.lock().unwrap();
        state.clients.push(sender);
        state.on_connect.clone()
    };
    for frame in greeting {
        socket.send(Message::Text(frame.to_string())).await?;
    }
    loop {
        tokio::select! {
            frame = receiver.recv() => match frame {
                Some(Some(frame)) => socket.send(Message::Text(frame)).await?,
                _ => { let _ = socket.close(None).await; return Ok(()); },
            },
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    state.lock().unwrap().requests.push(json!({"method": "WS_IN", "frame": text}));
                },
                Some(Ok(_)) => {},
                _ => return Ok(()),
            },
        }
    }
}
