//! Translate the already admitted conversation when a live binding changes wire protocol.
//! Signatures and private reasoning blocks are local to the original provider.
use super::super::{
    anthropic_client::{AnthropicContent, AnthropicMessage, ContentBlock},
    openai_client::OpenAIMessage,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};

pub(super) fn anthropic(messages: Vec<OpenAIMessage>) -> Result<Vec<AnthropicMessage>> {
    let mut converted = Vec::new();
    for message in messages {
        let content = serde_json::to_value(message.content)?;
        if message.role == "tool" {
            converted.push(AnthropicMessage {
                role: "user".into(),
                content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: message.tool_call_id.context("Missing tool result ID")?,
                    content,
                    is_error: None,
                }]),
            });
            continue;
        }
        let content = super::multimodal::anthropic_content(&content)?;
        let mut blocks = match content {
            AnthropicContent::Text(text) if text.is_empty() => Vec::new(),
            AnthropicContent::Text(text) => vec![ContentBlock::Text { text }],
            AnthropicContent::Blocks(blocks) => blocks,
        };
        for call in message.tool_calls.unwrap_or_default() {
            blocks.push(ContentBlock::ToolUse {
                id: call.id,
                name: call.function.name,
                input: serde_json::from_str(&call.function.arguments)?,
            });
        }
        if !blocks.is_empty() {
            converted.push(AnthropicMessage {
                role: message.role,
                content: AnthropicContent::Blocks(blocks),
            });
        }
    }
    Ok(converted)
}

pub(super) fn openai(messages: Vec<AnthropicMessage>) -> Result<Vec<OpenAIMessage>> {
    let mut converted = Vec::new();
    for message in messages {
        let blocks = match message.content {
            AnthropicContent::Text(text) => vec![ContentBlock::Text { text }],
            AnthropicContent::Blocks(blocks) => blocks,
        };
        let mut parts = Vec::new();
        let mut calls = Vec::new();
        for block in blocks {
            match block {
                ContentBlock::ToolUse { id, name, input } => calls.push(json!({"id":id,"type":"function","function":{"name":name,"arguments":serde_json::to_string(&input)?}})),
                ContentBlock::ToolResult { tool_use_id, content, .. } => {
                    let content = if content.is_string() || content.is_array() { content } else { Value::String(content.to_string()) };
                    let content = super::multimodal::openai_content(&content)?;
                    converted.push(serde_json::from_value(json!({"role":"tool","tool_call_id":tool_use_id,"content":content}))?);
                }
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => (),
                other => parts.push(serde_json::to_value(other)?),
            }
        }
        if !parts.is_empty() || !calls.is_empty() {
            let content = super::multimodal::openai_content(&Value::Array(parts))?;
            let mut value = json!({"role":message.role,"content":content});
            if !calls.is_empty() {
                value["tool_calls"] = Value::Array(calls);
            }
            converted.push(serde_json::from_value(value)?);
        }
    }
    Ok(converted)
}
