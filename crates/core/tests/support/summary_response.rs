//! Local provider reply in the encoding requested by the summarizer.
use axum::{
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

pub fn reply(message: Value, streaming: bool) -> Response {
    if !streaming {
        return Json(message).into_response();
    }
    let frames = if message["type"] == "message" {
        vec![
            json!({"type":"message_start","message":{"id":"r","usage":{"input_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":message["content"][0]["text"]}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":message["stop_reason"]},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ]
    } else {
        vec![
            json!({"choices":[{"index":0,"delta":{"content":message["choices"][0]["message"]["content"]},"finish_reason":message["choices"][0]["finish_reason"]}],"usage":message["usage"]}),
        ]
    };
    let mut data: String = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    if message["type"] != "message" {
        data.push_str("data: [DONE]\n\n");
    }
    ([("content-type", "text/event-stream")], data).into_response()
}
