//! Compaction has no assistant segment of its own. Attach its numeric metrics
//! to the existing history row and keep its HTTP request ID for recording.
use super::{provider_manager::ResolvedProvider, request_options::RequestOptions};
use crate::db::session_store::SessionDb;
use anyhow::Result;
use serde_json::{json, Value};

#[derive(Clone)]
pub(crate) struct MetricsStore(pub SessionDb);

impl std::fmt::Debug for MetricsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MetricsStore").field(&self.0.id()).finish()
    }
}

pub(crate) struct CompactionMetrics {
    store: SessionDb,
    snapshot: Value,
    message: String,
}

impl CompactionMetrics {
    pub(crate) fn start(
        options: &mut RequestOptions,
        provider: &ResolvedProvider,
        api: &str,
    ) -> Result<Option<Self>> {
        let Some(store) = &options.compaction_metrics else {
            return Ok(None);
        };
        let id = uuid::Uuid::new_v4().to_string();
        options.model_request_id = Some(id.clone());
        let snapshot = json!({
            "id":id, "providerId":provider.provider.id, "model":provider.model.id,
            "apiFormat":api, "startedAt":chrono::Utc::now().timestamp_millis(), "status":"started"
        });
        let metadata = json!({
            "kind": "compaction",
            "stage": options.compaction_stage,
            "label": options.compaction_label,
        });
        let message = store
            .0
            .write(|w| w.start_compaction_metrics(&snapshot, &metadata))?;
        Ok(message.map(|message| Self {
            store: store.0.clone(),
            snapshot,
            message,
        }))
    }

    pub(crate) fn observe_usage(&mut self, usage: &Value) {
        let snapshot = &mut self.snapshot;
        let cached = usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| usage["input_tokens_details"]["cached_tokens"].as_u64())
            .or_else(|| usage["prompt_cache_hit_tokens"].as_u64())
            .or_else(|| usage["cached_tokens"].as_u64())
            .or_else(|| usage["cache_read_input_tokens"].as_u64());
        let input = usage["prompt_tokens"].as_u64().or_else(|| {
            usage["input_tokens"].as_u64().map(|n| {
                if snapshot["apiFormat"] == "anthropic-messages" {
                    n.saturating_add(cached.unwrap_or_default()).saturating_add(
                        usage["cache_creation_input_tokens"]
                            .as_u64()
                            .unwrap_or_default(),
                    )
                } else {
                    n
                }
            })
        });
        snapshot["inputTokens"] = json!(input);
        snapshot["cacheReadTokens"] = json!(cached);
        let output = usage["completion_tokens"]
            .as_u64()
            .or_else(|| usage["output_tokens"].as_u64());
        let total = usage["total_tokens"].as_u64().or_else(|| {
            input
                .zip(output)
                .map(|(input, output)| input.saturating_add(output))
        });
        snapshot["outputTokens"] = json!(output);
        snapshot["cacheWriteTokens"] = json!(super::request_trace::cache_write_tokens(usage));
        snapshot["totalTokens"] = json!(total);
        snapshot["usage"] = usage.clone();
        // Session usage and pricing use prompt_tokens to identify an inclusive
        // input count. Keep the provider-specific fields for inspection as well.
        if usage.get("input_tokens").is_some() && usage.get("prompt_tokens").is_none() {
            snapshot["usage"]["prompt_tokens"] = json!(input);
        }
    }

    pub(crate) fn finish(mut self, usage: &Value, reason: &str, status: &str) -> Result<()> {
        self.observe_usage(usage);
        self.snapshot["status"] = json!(status);
        self.snapshot["finishReason"] = json!(reason);
        self.snapshot["endedAt"] = json!(chrono::Utc::now().timestamp_millis());
        self.store
            .write(|w| w.finish_compaction_metrics(&self.message, &self.snapshot))
    }
}

impl Drop for CompactionMetrics {
    fn drop(&mut self) {
        // Cancellation or transport failure can end the operation before a
        // provider reports usage. Leave both counters nullable.
        if self.snapshot["status"] == "started" {
            self.snapshot["status"] = json!("incomplete");
            self.snapshot["endedAt"] = json!(chrono::Utc::now().timestamp_millis());
            if let Err(error) = self
                .store
                .write(|w| w.finish_compaction_metrics(&self.message, &self.snapshot))
            {
                tracing::warn!(%error, "Could not finalize compaction request metrics");
            }
        }
    }
}
