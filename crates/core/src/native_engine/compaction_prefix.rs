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
}

impl Source {
    pub(super) fn new(messages: &[Value]) -> Self {
        Self {
            count: messages.len(),
            digest: digest(messages),
        }
    }

    fn suffix<'a>(&self, history: &'a [Value]) -> Option<&'a [Value]> {
        let (base, tail) = history.split_at_checked(self.count)?;
        (digest(base) == self.digest).then_some(tail)
    }
}

#[derive(Clone)]
pub(super) struct Capture {
    pub session: SessionDb,
    pub route: String,
    pub source: Source,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SavedPrefix {
    route: String,
    session: String,
    #[serde(default)]
    complete_source: bool,
    source: Source,
    wire: WireRequest,
}

impl Capture {
    pub fn save(&self, wire: &WireRequest) -> Result<()> {
        let saved = SavedPrefix {
            route: self.route.clone(),
            session: self.session.id().into(),
            complete_source: true,
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
    if !saved.complete_source || saved.session != session.id() || saved.route != route(provider) {
        return Ok(None);
    }
    let Some(tail) = saved.source.suffix(history) else {
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
    fn prefix_requires_the_original_contents_of_every_covered_row() {
        let history = vec![
            json!({"role":"tool","tool_call_id":"first","content":"x".repeat(1100)}),
            json!({"role":"user","content":"continue"}),
        ];
        let source = Source::new(&history[..1]);
        assert_eq!(source.suffix(&history), Some(&history[1..]));
        let mut shortened = history.clone();
        crate::services::compaction::microcompact::clear_tool_results(&mut shortened);
        assert!(Source::new(&shortened).suffix(&history).is_none());
    }
}
