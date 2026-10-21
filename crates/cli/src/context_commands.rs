//! Inspection only: the engine captures accounting at the actual transport boundary.
use crate::session::Session;
use anyhow::Result;

pub fn show(session: &Session) -> Result<()> {
    eprintln!(
        "{}",
        render(claude_core::services::context_usage::last_request(
            &session.store.db,
            &session.id
        )?)
    );
    Ok(())
}
fn render(request: Option<claude_core::services::context_usage::LastRequest>) -> String {
    let mut lines = Vec::new();
    let Some(request) = request else {
        lines.push(format!(
            "Context: unknown. No recorded parent-session request is available."
        ));
        return lines.join("\n");
    };
    let time = request
        .started_at_ms
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.to_rfc3339())
        .unwrap_or_else(|| "unknown time".into());
    lines.push(format!(
        "Last request attempt: {} · {time} · {}",
        request.model, request.status
    ));
    lines.push(format!(
        "Provider-reported input: {}",
        count(request.reported_input_tokens)
    ));
    let Some(usage) = request.usage else {
        lines.push(format!(
            "Context breakdown: unknown. This request has no transport accounting metadata."
        ));
        lines.push(format!(
            "Historical request only; this is not the current or next request budget."
        ));
        return lines.join("\n");
    };
    lines.push(format!(
        "Estimated history: {} tokens",
        usage.history.estimated_tokens
    ));
    lines.push(format!(
        "Estimated system/developer instructions: {} tokens",
        usage.instructions.estimated_tokens
    ));
    lines.push(format!(
        "Estimated tool definitions: {} tokens",
        usage.tools.estimated_tokens
    ));
    lines.push(format!(
        "Estimated output schema: {} tokens",
        usage.output_schema.estimated_tokens
    ));
    lines.push(format!(
        "Estimated input total: {}",
        count(usage.estimated_input_tokens)
    ));
    if usage.unknown_parts() > 0 {
        lines.push(format!(
            "Known text subtotal: {} tokens; {} media/opaque parts have unknown token cost.",
            usage.text_subtotal(),
            usage.unknown_parts()
        ));
    }
    lines.push(format!(
        "Output reserve in request: {}",
        count(usage.sent_output_reserve)
    ));
    if usage.sent_output_reserve.is_none() {
        lines.push(format!(
            "Requested output budget: {} tokens; the protocol did not send an explicit limit.",
            usage.requested_output_budget
        ));
    }
    lines.push(format!(
        "Configured context window: {}",
        count(usage.configured_context_window)
    ));
    if let (Some(input), Some(reserve), Some(window)) = (
        request
            .reported_input_tokens
            .or(usage.estimated_input_tokens),
        usage.sent_output_reserve,
        usage.configured_context_window.filter(|window| *window > 0),
    ) {
        let used = input.saturating_add(reserve);
        lines.push(format!(
            "{} input + output reserve: {used} / {window} ({:.1}%)",
            if request.reported_input_tokens.is_some() {
                "Reported"
            } else {
                "Estimated"
            },
            used as f64 * 100.0 / window as f64
        ));
        if used > window {
            lines.push(format!(
                "Exceeds the configured window by {} tokens.",
                used - window
            ));
        }
    }
    lines.push(format!("Estimates use serialized UTF-8 bytes / 3, rounded up per category; provider tokenization and protocol overhead can differ."));
    lines.push(format!("Historical request only; later responses, model/settings changes and compaction are not included."));
    lines.join("\n")
}
fn count(value: Option<u64>) -> String {
    value
        .map(|n| format!("{n} tokens"))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_core::{
        native_engine::request_context::ContextUsage, services::context_usage::LastRequest,
    };
    use serde_json::json;

    #[test]
    fn context_output_distinguishes_unknown_estimates_and_last_request() {
        assert!(render(None).contains("Context: unknown"));
        let request = LastRequest {
            model: "previous-model".into(),
            started_at_ms: Some(0),
            status: "completed".into(),
            reported_input_tokens: Some(90),
            usage: Some(ContextUsage::from_body(
                "openai-completions",
                &json!({"messages":[{"role":"system","content":"PRIVATE_SYSTEM"},{"role":"user","content":"PRIVATE_USER"}],"max_tokens":200}),
                Some(1000),
                400,
            )),
        };
        let text = render(Some(request));
        assert!(text.contains("Last request attempt: previous-model"));
        assert!(text.contains("Provider-reported input: 90 tokens"));
        assert!(text.contains("Reported input + output reserve: 290 / 1000 (29.0%)"));
        assert!(text.contains("Estimated system/developer instructions:"));
        assert!(text.contains("Historical request only"));
        assert!(!text.contains("PRIVATE"));
        assert!(!text.contains('\x1b'));
        let request = LastRequest {
            model: "model".into(),
            started_at_ms: None,
            status: "error".into(),
            reported_input_tokens: None,
            usage: Some(ContextUsage::from_body(
                "openai-responses",
                &json!({"input":[{"type":"input_image","image_url":"PRIVATE_DATA"}]}),
                None,
                400,
            )),
        };
        let text = render(Some(request));
        for expected in [
            "Provider-reported input: unknown",
            "Estimated input total: unknown",
            "Output reserve in request: unknown",
            "Configured context window: unknown",
            "unknown token cost",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains('%'));
    }
}
