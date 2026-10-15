//! Numeric accounting of the actual transport body, never a reconstructed prompt.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Estimate {
    pub serialized_utf8_bytes: u64,
    pub estimated_tokens: u64,
    pub unknown_parts: u64,
}
impl Estimate {
    fn add(&mut self, value: &Value, opaque: bool) {
        if value.is_null() {
            return;
        }
        self.serialized_utf8_bytes = self.serialized_utf8_bytes.saturating_add(bytes(
            value,
            opaque,
            &mut self.unknown_parts,
        ));
        self.estimated_tokens = self.serialized_utf8_bytes.div_ceil(3);
    }
}

// Binary media and opaque reasoning do not tokenize like their wire encoding.
// Restrict this recognition to message bodies, never schemas or tool arguments.
fn bytes(value: &Value, opaque: bool, unknown: &mut u64) -> u64 {
    match value {
        Value::Object(map) => {
            if opaque
                && (matches!(
                    map.get("type").and_then(Value::as_str),
                    Some(
                        "image"
                            | "image_url"
                            | "input_image"
                            | "input_audio"
                            | "audio"
                            | "document"
                            | "input_file"
                            | "video"
                            | "redacted_thinking"
                    )
                ) || [
                    "inlineData",
                    "inline_data",
                    "fileData",
                    "file_data",
                    "image",
                    "document",
                    "video",
                    "audio",
                ]
                .iter()
                .any(|k| map.contains_key(*k)))
            {
                *unknown = unknown.saturating_add(1);
                return 0;
            }
            let mut result: u64 = 2;
            for (key, item) in map {
                if opaque
                    && matches!(
                        key.as_str(),
                        "encrypted_content" | "signature" | "thoughtSignature"
                    )
                {
                    if !item.is_null() {
                        *unknown = unknown.saturating_add(1);
                    }
                    continue;
                }
                // Tool arguments/results may themselves contain keys such as image.
                let nested_opaque = opaque
                    && !matches!(
                        key.as_str(),
                        "input" | "arguments" | "args" | "response" | "output"
                    );
                if result > 2 {
                    result = result.saturating_add(1);
                }
                result = result
                    .saturating_add(serde_json::to_string(key).map_or(0, |s| s.len() as u64) + 1);
                result = result.saturating_add(bytes(item, nested_opaque, unknown));
            }
            result
        }
        Value::Array(items) => items.iter().fold(
            2u64.saturating_add(items.len().saturating_sub(1) as u64),
            |sum, item| sum.saturating_add(bytes(item, opaque, unknown)),
        ),
        _ => serde_json::to_string(value).map_or(0, |s| s.len() as u64),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ContextUsage {
    pub version: u32,
    pub history: Estimate,
    pub instructions: Estimate,
    pub tools: Estimate,
    pub output_schema: Estimate,
    pub configured_context_window: Option<u64>,
    pub requested_output_budget: u64,
    pub sent_output_reserve: Option<u64>,
    pub estimated_input_tokens: Option<u64>,
}
impl ContextUsage {
    pub fn from_body(protocol: &str, body: &Value, window: Option<u32>, requested: u32) -> Self {
        let mut usage = Self {
            version: 1,
            history: Estimate::default(),
            instructions: Estimate::default(),
            tools: Estimate::default(),
            output_schema: Estimate::default(),
            configured_context_window: window.filter(|w| *w > 0).map(u64::from),
            requested_output_budget: requested.into(),
            sent_output_reserve: None,
            estimated_input_tokens: None,
        };
        let (messages, system, tools, schema, reserve) = match protocol {
            "openai-completions" => (
                &body["messages"],
                &Value::Null,
                &body["tools"],
                &body["response_format"],
                body["max_completion_tokens"]
                    .as_u64()
                    .or_else(|| body["max_tokens"].as_u64()),
            ),
            "anthropic-messages" => (
                &body["messages"],
                &body["system"],
                &body["tools"],
                &body["output_config"]["format"],
                body["max_tokens"].as_u64(),
            ),
            "openai-responses" => (
                &body["input"],
                &body["instructions"],
                &body["tools"],
                &body["text"]["format"],
                body["max_output_tokens"].as_u64(),
            ),
            "google-generative-ai" => (
                &body["contents"],
                &body["systemInstruction"],
                &body["tools"],
                &body["generationConfig"]["responseSchema"],
                body["generationConfig"]["maxOutputTokens"].as_u64(),
            ),
            "bedrock-converse-stream" => (
                &body["messages"],
                &body["system"],
                &body["toolConfig"],
                &Value::Null,
                body["inferenceConfig"]["maxTokens"].as_u64(),
            ),
            _ => {
                usage.history.unknown_parts = 1;
                return usage;
            }
        };
        if let Some(messages) = messages.as_array() {
            for message in messages {
                if matches!(message["role"].as_str(), Some("system" | "developer")) {
                    usage.instructions.add(message, true);
                } else {
                    usage.history.add(message, true);
                }
            }
        } else {
            usage.history.add(messages, true);
        }
        usage.instructions.add(system, true);
        usage.tools.add(tools, false);
        usage.tools.add(&body["tool_choice"], false);
        usage.output_schema.add(schema, false);
        usage.sent_output_reserve = reserve;
        if usage.unknown_parts() == 0 {
            usage.estimated_input_tokens = Some(usage.text_subtotal());
        }
        usage
    }
    pub fn text_subtotal(&self) -> u64 {
        [
            &self.history,
            &self.instructions,
            &self.tools,
            &self.output_schema,
        ]
        .iter()
        .fold(0u64, |n, e| n.saturating_add(e.estimated_tokens))
    }
    pub fn unknown_parts(&self) -> u64 {
        [
            &self.history,
            &self.instructions,
            &self.tools,
            &self.output_schema,
        ]
        .iter()
        .fold(0u64, |n, e| n.saturating_add(e.unknown_parts))
    }
}

/// Per-request capture: never shared across simultaneous child requests.
#[derive(Clone, Default)]
pub struct ContextCapture {
    pub(super) latest: Arc<Mutex<Option<ContextUsage>>>,
    pub(super) event: Option<(
        tokio::sync::mpsc::Sender<super::tool_loop::EngineEvent>,
        Value,
    )>,
}
impl ContextCapture {
    pub fn snapshot(&self) -> Option<ContextUsage> {
        self.latest.lock().ok()?.clone()
    }
    pub async fn record(&self, protocol: &str, body: &Value, window: Option<u32>, requested: u32) {
        let usage = ContextUsage::from_body(protocol, body, window, requested);
        if let Ok(mut current) = self.latest.lock() {
            *current = Some(usage.clone());
        }
        if let Some((sender, initial)) = &self.event {
            let mut snapshot = initial.clone();
            snapshot["contextUsage"] = serde_json::to_value(usage).unwrap_or(Value::Null);
            let _ = sender
                .send(super::tool_loop::EngineEvent::RequestSnapshot(snapshot))
                .await;
        }
    }
}

#[cfg(test)]
#[path = "request_context_tests.rs"]
mod tests;
