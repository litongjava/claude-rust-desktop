use super::super::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use deepseek_code_core::db::session_store::H5Session;
use serde::Deserialize;
use serde_json::{json, Value};

async fn authorize(state: &AppState, token: String) -> Result<H5Session, StatusCode> {
    let db = state.db_manager.clone();
    tokio::task::spawn_blocking(move || db.authorize_h5(&token))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|e| {
            tracing::error!("H5 authorization failed: {e:#}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::UNAUTHORIZED)
}
async fn h5_access_handler(
    Path(token): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let grant = match authorize(&state, token).await {
        Ok(g) => g,
        Err(code) => {
            return (code, Json(json!({"error":"Invalid or expired token"}))).into_response()
        }
    };
    match tokio::task::spawn_blocking(move||grant.messages().map(|messages|json!({"success":true,"conversation":grant.conversation,"messages":messages}))).await {
        Ok(Ok(value))=>Json(value).into_response(),
        _=>(StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":"Failed to fetch conversation"}))).into_response(),
    }
}
#[derive(Deserialize)]
struct H5ChatRequest {
    message: String,
    model: Option<String>,
}

async fn h5_chat_handler(
    Path(token): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<H5ChatRequest>,
) -> impl IntoResponse {
    let grant = match authorize(&state, token).await {
        Ok(g) => g,
        Err(code) => {
            return (code, Json(json!({"error":"Invalid or expired token"}))).into_response()
        }
    };
    let id = grant.conversation.id.clone();
    let _gate = super::input_queue::gate(&state, &id).await;
    if state.generation_runs.is_active(&id) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Conversation is running"})),
        )
            .into_response();
    }
    let request = crate::native_engine::engine_core::ChatRequest {
        conversation_id: id,
        messages: vec![json!({"role":"user","content":req.message})],
        model: req.model.unwrap_or_else(|| "claude-sonnet-4-6".into()),
        context_boundary: None,
        provider_id: None,
        assistant_message_id: None,
        effort: None,
        system_prompt: None,
        standalone_frontend: false,
        max_tokens: None,
        max_turns: None,
        response_schema: None,
        workspace_path: None,
        temperature: None,
        top_p: None,
        reasoning_mode: None,
    };
    super::chat::start_authorized_chat(state, request, None, Some(grant)).await
}

pub(super) fn client_event(value: Value) -> Option<Value> {
    let fields: &[&str] = match value["type"].as_str()? {
        "request_snapshot"
        | "ask_user"
        | "tool_arg_delta"
        | "message_delta"
        | "pipeline_tool_result"
        | "context_reset" => return None,
        "message_start" => &["model"],
        "tool_use_start" => &["tool_use_id", "tool_name", "tool_input"],
        "tool_use_done" => &[
            "tool_use_id",
            "tool_name",
            "tool_input",
            "output",
            "is_error",
        ],
        "tool_permission" => &[
            "toolscript",
            "request_id",
            "tool_use_id",
            "tool_name",
            "input",
        ],
        "queued_input" => {
            return Some(
                json!({"type":"queued_input","text":{"text":value["text"],"content":value["content"],"origin":value["origin"]}}),
            )
        }
        _ => return Some(value),
    };
    let mut event = json!({"type":value["type"]});
    for &field in fields {
        event[field] = value[field].clone();
    }
    Some(event)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/h5/access/{token}", get(h5_access_handler))
        .route("/api/h5/access/{token}/chat", post(h5_chat_handler))
}
