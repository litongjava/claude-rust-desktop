//! Follow-ups start with the provider's final wire body, including cache markers.
use super::{
    anthropic_client::AnthropicClient, openai_client::OpenAIClient,
    provider_manager::ResolvedProvider, provider_runtime, request_options::RequestOptions,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct WireRequest {
    pub api: String,
    pub body: Value,
    pub builtin_vision: Option<bool>,
}

impl WireRequest {
    pub fn messages_key(&self) -> &'static str {
        match self.api.as_str() {
            "openai-responses" => "input",
            "google-generative-ai" => "contents",
            _ => "messages",
        }
    }

    pub fn append_instruction(&mut self, instruction: &str) -> Result<()> {
        let message = match self.api.as_str() {
            "openai-completions" | "anthropic-messages" => {
                json!({"role":"user","content":instruction})
            }
            "openai-responses" => {
                json!({"role":"user","content":[{"type":"input_text","text":instruction}]})
            }
            "google-generative-ai" => json!({"role":"user","parts":[{"text":instruction}]}),
            "bedrock-converse-stream" => json!({"role":"user","content":[{"text":instruction}]}),
            api => anyhow::bail!("Unknown follow-up protocol {api}"),
        };
        let key = self.messages_key();
        self.body[key]
            .as_array_mut()
            .context("Missing wire message list")?
            .push(message);
        Ok(())
    }

    pub(super) fn allow_text_summary(&mut self) {
        let body = &mut self.body;
        if let Some(fields) = body.as_object_mut() {
            fields.remove("response_format");
            if fields.get("tool_choice").is_some_and(|choice| {
                choice != "auto"
                    && choice != "none"
                    && choice["type"] != "auto"
                    && choice["type"] != "none"
                    && choice["mode"] != "auto"
            }) {
                fields.remove("tool_choice");
            }
        }
        for parent in ["text", "output_config"] {
            if let Some(fields) = body.get_mut(parent).and_then(Value::as_object_mut) {
                fields.remove("format");
                if fields.is_empty() {
                    body.as_object_mut().unwrap().remove(parent);
                }
            }
        }
        if let Some(config) = body
            .get_mut("generationConfig")
            .and_then(Value::as_object_mut)
        {
            for field in ["responseMimeType", "responseSchema", "responseJsonSchema"] {
                config.remove(field);
            }
        }
        if let Some(config) = body.get_mut("toolConfig").and_then(Value::as_object_mut) {
            if config
                .get("toolChoice")
                .is_some_and(|choice| choice.get("auto").is_none())
            {
                config.remove("toolChoice");
            }
            if let Some(choice) = config.get_mut("functionCallingConfig") {
                if choice["mode"] == "ANY" {
                    *choice = json!({"mode":"AUTO"});
                }
            }
        }
    }

    pub fn set_output_budget(&mut self, budget: u32) {
        // Some transports omit the field intentionally (Codex Responses).
        for path in [
            "/max_tokens",
            "/max_completion_tokens",
            "/max_output_tokens",
            "/generationConfig/maxOutputTokens",
            "/inferenceConfig/maxTokens",
        ] {
            if let Some(value) = self.body.pointer_mut(path) {
                *value = json!(budget);
            }
        }
    }

    pub fn direct_anthropic(&self) -> bool {
        self.builtin_vision.is_none() && self.api == "anthropic-messages"
    }

    pub async fn stream(
        self,
        provider: &ResolvedProvider,
        options: RequestOptions,
    ) -> Result<provider_runtime::ProviderStream> {
        let budget = if options.purpose == "suggestion" {
            crate::performance::AttemptBudget::single_attempt()
        } else {
            crate::performance::AttemptBudget::configured()
        };
        let (stream, _) = if let Some(vision) = self.builtin_vision {
            let client = if budget.is_single_post() {
                super::model_http::single_post_client()?
            } else {
                super::model_http::streaming_client(super::model_http::Redirect::None)?
            };
            provider_runtime::stream_body(
                provider, self.body, "user", vision, 0, None, &options, client, budget,
            )
            .await?
        } else if self.direct_anthropic() {
            AnthropicClient::new()
                .with_request_options(options)
                .stream_body(provider, self.body, 0, None)
                .await?
        } else {
            OpenAIClient::new()
                .with_request_options(options)
                .stream_body(provider, self.body, 0, None, budget)
                .await?
        };
        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_preserves_system_text_and_permits_a_free_text_answer() {
        let schema = json!({"type":"boolean"});
        for mut body in [
            json!({"system":"Original"}),
            json!({"system":[{"type":"text","text":"Original"}]}),
            json!({"messages":[{"role":"system","content":"Original"}]}),
        ] {
            let original = body.clone();
            if body.get("system").is_some() {
                crate::structured_output::append_anthropic_instruction(&mut body, &schema);
            } else {
                crate::structured_output::append_openai_instruction(&mut body, &schema);
            }
            crate::structured_output::remove_prompt_instruction(&mut body, &schema);
            assert_eq!(body, original);
        }
        for choice in [
            json!("required"),
            json!({"type":"tool","name":"Write"}),
            json!({"type":"any"}),
        ] {
            let mut wire = WireRequest {
                api: "anthropic-messages".into(),
                builtin_vision: None,
                body: json!({"tools":[{"name":"Write"}],"tool_choice":choice, "output_config":{"format":schema,"effort":"high"},"text":{"format":schema}}),
            };
            wire.allow_text_summary();
            assert_eq!(
                wire.body,
                json!({"tools":[{"name":"Write"}],"output_config":{"effort":"high"}})
            );
        }
    }
}
