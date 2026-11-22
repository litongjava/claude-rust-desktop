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
