//! One replaceable main-request prefix per session. Keep its source position
//! separate from the wire representation: reminders, tool IDs and provider
//! cache markers need not appear in the stored transcript.
use super::{
    followup::WireRequest, openai_client::OpenAIMessage, provider_manager::ResolvedProvider,
    provider_runtime,
};
use crate::db::session_store::{settings::COMPACTION_PREFIX, SessionDb};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn digest(messages: &[Value]) -> String {
    let mut hash = Sha256::new();
    serde_json::to_writer(&mut hash, messages).expect("JSON values serialize");
    format!("{:x}", hash.finalize())
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Source {
    count: usize,
    digest: String,
    cleared_through: usize,
}

impl Source {
    pub(super) fn new(messages: &[Value]) -> Self {
        let cleared_through = messages
            .iter()
            .rposition(crate::services::compaction::microcompact::has_cleared_result)
            .map_or(0, |i| i + 1);
        Self {
            count: messages.len(),
            digest: digest(messages),
            cleared_through,
        }
    }

    fn suffix<'a>(&self, history: &'a [Value], last_tool: Option<&str>) -> Option<&'a [Value]> {
        let (base, tail) = history.split_at_checked(self.count)?;
        if digest(base) != self.digest {
            if self.cleared_through == 0 {
                return None;
            }
            let mut replay = base.to_vec();
            crate::services::compaction::microcompact::clear_tool_results(
                replay.get_mut(..self.cleared_through)?,
            );
            if digest(&replay) != self.digest {
                return None;
            }
        }
        let Some(tool) = last_tool else {
            return Some(tail);
        };
        // Session replay namespaces call IDs by the assistant message ID.
        let matches = |id: &Value| {
            id.as_str().is_some_and(|id| {
                id == tool
                    || id
                        .strip_suffix(tool)
                        .is_some_and(|prefix| prefix.ends_with(':'))
            })
        };
        let end = tail.iter().position(|m| {
            matches(&m["tool_call_id"])
                || m["content"].as_array().is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b["type"] == "tool_result" && matches(&b["tool_use_id"]))
                })
        })?;
        Some(&tail[end + 1..])
    }
}

#[derive(Clone)]
pub(super) struct Capture {
    pub session: SessionDb,
    pub route: String,
    pub last_tool: Option<String>,
    pub source: Source,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SavedPrefix {
    route: String,
    session: String,
    last_tool: Option<String>,
    source: Source,
    wire: WireRequest,
}

impl Capture {
    pub fn save(&self, wire: &WireRequest) -> Result<()> {
        let saved = SavedPrefix {
            route: self.route.clone(),
            session: self.session.id().into(),
            last_tool: self.last_tool.clone(),
            source: self.source.clone(),
            wire: wire.clone(),
        };
        self.session
            .write(|w| w.set_setting(COMPACTION_PREFIX, &saved))
    }
}

pub(super) fn route(provider: &ResolvedProvider) -> String {
    let p = &provider.provider;
    let route = serde_json::json!([
        p.id,
        p.base_url.trim_end_matches('/'),
        p.api_format,
        p.builtin_id,
        provider.model.api,
        provider.model.id
    ]);
    format!("{:x}", Sha256::digest(route.to_string()))
}

pub(super) fn load(
    session: &SessionDb,
    provider: &ResolvedProvider,
    history: &[Value],
) -> Result<Option<WireRequest>> {
    let Some(saved) = session.read(|r| r.setting(COMPACTION_PREFIX))? else {
        return Ok(None);
    };
    if saved.session != session.id() || saved.route != route(provider) {
        return Ok(None);
    }
    let Some(tail) = saved.source.suffix(history, saved.last_tool.as_deref()) else {
        return Ok(None);
    };
    let mut wire = saved.wire;
    let key = wire.messages_key();
    if !tail.is_empty() {
        let messages = if wire.direct_anthropic() {
            let messages = tail
                .iter()
                .map(|m| {
                    Ok(super::anthropic_client::AnthropicMessage {
                        role: m["role"].as_str().context("Missing role")?.into(),
                        content: super::tool_loop::multimodal::anthropic_content(&m["content"])?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            serde_json::to_value(super::anthropic_client::merge_consecutive_roles(messages))?
        } else {
            let messages = super::tool_loop::multimodal::normalize_messages(tail)?
                .into_iter()
                .map(serde_json::from_value::<OpenAIMessage>)
                .collect::<serde_json::Result<Vec<_>>>()?;
            let mut body = provider_runtime::messages::request(
                &wire.api,
                provider,
                &Value::Null,
                messages,
                None,
                vec![],
                1,
            )?;
            body[key].take()
        };
        wire.body[key]
            .as_array_mut()
            .context("Missing prefix messages")?
            .extend(
                messages
                    .as_array()
                    .context("Missing continuation messages")?
                    .iter()
                    .cloned(),
            );
    }
    Ok(Some(wire))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prefix_requires_matching_history_and_the_last_included_tool_result() {
        let mut history = vec![
            json!({"role":"tool","tool_call_id":"first","content":"x".repeat(1100)}),
            json!({"role":"user","content":"continue"}),
        ];
        let mut main_input = history.clone();
        crate::services::compaction::microcompact::clear_tool_results(&mut main_input[..1]);
        let source = Source::new(&main_input);
        history.extend([
            json!({"role":"tool","tool_call_id":"assistant:last","content":"result"}),
            json!({"role":"assistant","content":"final answer"}),
        ]);
        assert_eq!(source.suffix(&history, Some("last")), Some(&history[3..]));
        assert!(source.suffix(&history, Some("unknown")).is_none());
        history[1]["content"] = json!("rewritten");
        assert!(source.suffix(&history, None).is_none());
    }
}
