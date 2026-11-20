//! Retry a disconnected model stream only before any content is exposed.
use super::request_identity::RequestIdentity;
use crate::performance::{AttemptBudget, AttemptCounter, RequestFailure};
use anyhow::Result;
use futures::{Stream, StreamExt};
use serde_json::Value;
#[cfg(test)]
use std::time::Duration;
use std::{future::Future, pin::Pin};

/// One decoded response stream, already normalized to SSE text.
pub(super) type DecodedStream = Pin<Box<dyn Stream<Item = Result<String>> + Send>>;

/// One HTTP attempt. The future owns everything it needs, so an opener can be
/// invoked again without borrowing its closure across an await.
pub(super) type OpenFuture = Pin<Box<dyn Future<Output = Result<reqwest::Response>> + Send>>;

/// A decoder saw data it must not duplicate, so a body failure is final for
/// this request. The original transport error stays in the chain, so the
/// structured failure keeps its real classification and cause.
#[derive(Debug)]
pub(super) struct NoReplay {
    error: reqwest::Error,
    /// The decoder had already seen the provider's terminal marker.
    terminal: bool,
}

impl NoReplay {
    pub(super) fn new(error: reqwest::Error) -> Self {
        Self {
            error,
            terminal: false,
        }
    }

    /// The body stopped after the decoder latched the provider's terminal
    /// marker. The observation is independent of the timeout classification.
    pub(super) fn with_terminal_marker(error: reqwest::Error, terminal: bool) -> Self {
        Self { error, terminal }
    }

    pub(super) fn terminal_marker(&self) -> bool {
        self.terminal
    }
}

impl std::fmt::Display for NoReplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("provider stream ended after data that cannot be replayed")
    }
}

impl std::error::Error for NoReplay {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

async fn send<O>(
    open: &mut O,
    identity: &RequestIdentity,
    budget: &AttemptBudget,
    attempts: &AttemptCounter,
) -> Result<reqwest::Response>
where
    O: FnMut(&RequestIdentity) -> OpenFuture,
{
    let result = crate::performance::retry_with_backoff_budget(
        || {
            // Building the future is synchronous and owns its own state; the
            // mutable borrow of the opener ends before the attempt is awaited.
            let response = open(identity);
            async move {
                // A status belongs to the attempt that is about to start; an
                // older response must not label a connection that never opened.
                attempts.clear_status();
                let response = response.await?;
                // The status of the attempt that produced this response stays
                // known, so a stream ending without a completion marker can
                // report it.
                attempts.set_status(response.status().as_u16());
                Ok(response)
            }
        },
        budget,
    )
    .await;
    // Record what the server really saw, including the failing attempt.
    attempts.set_attempts(budget.used());
    result
}

/// Retry one model request under the configured attempt budget.
///
/// `open` performs exactly one HTTP exchange. `decode` turns one response into
/// normalized SSE text and resets its parser state for every call. Send retries
/// and pre-content body retries draw from the same budget, so the configured
/// maximum is never multiplied. Empty handshakes are withheld so a retry cannot
/// duplicate observable content.
pub(super) async fn attempts<O, D>(
    identity: RequestIdentity,
    open: O,
    decode: D,
) -> Result<(DecodedStream, AttemptCounter)>
where
    O: FnMut(&RequestIdentity) -> OpenFuture + Send + 'static,
    D: Fn(reqwest::Response) -> DecodedStream + Send + 'static,
{
    attempts_with_budget(identity, open, decode, AttemptBudget::configured()).await
}

/// Retry one model request under a caller-supplied attempt budget.
///
/// The supplied budget is the authority for both the send retries and the
/// pre-content body retries. A single-attempt budget therefore performs exactly
/// one physical POST: no send retry and no pre-content reopen. Redirects and
/// reqwest's internal retries are the opener client's responsibility.
pub(super) async fn attempts_with_budget<O, D>(
    identity: RequestIdentity,
    mut open: O,
    decode: D,
    mut budget: AttemptBudget,
) -> Result<(DecodedStream, AttemptCounter)>
where
    O: FnMut(&RequestIdentity) -> OpenFuture + Send + 'static,
    D: Fn(reqwest::Response) -> DecodedStream + Send + 'static,
{
    budget.id = identity.id().to_owned();
    let attempts = AttemptCounter::default();
    let first = send(&mut open, &identity, &budget, &attempts).await?;
    let counter = attempts.clone();
    let total = budget.max();
    let stream = Box::pin(async_stream::try_stream! {
        let mut next = Some(first);
        for attempt in 0..total {
            let stream = decode(next.take().expect("attempt response"));
            futures::pin_mut!(stream);
            let mut pending = String::new();
            let mut unparsed = String::new();
            let mut exposed = false;
            let mut failure = None;
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => { failure = Some(error); break; }
                };
                if exposed {
                    yield chunk;
                    continue;
                }
                pending.push_str(&chunk);
                unparsed.push_str(&chunk);
                let parsed = crate::streaming::sse_parser::consume_sse_payloads(&unparsed);
                unparsed = parsed.remainder;
                // Unknown events, completion, errors and any content commit the stream.
                if pending.len() > 65_536 || parsed.payloads.iter().any(|event| !empty_prelude(event)) {
                    exposed = true;
                    unparsed.clear();
                    yield std::mem::take(&mut pending);
                }
            }
            let transport_failure = failure.as_ref().is_none_or(|error| {
                error.downcast_ref::<reqwest::Error>().is_some_and(|http| http.is_body() || http.is_decode() || http.is_timeout() || http.is_connect())
            });
            // A decoder that already withheld data refuses a retry regardless
            // of what the transport error alone would allow.
            let no_replay = failure.as_ref().is_some_and(|error| {
                error
                    .chain()
                    .any(|cause| cause.downcast_ref::<NoReplay>().is_some())
            });
            // The decoder's raw terminal observation survives a later body stop,
            // independently of the timeout classification.
            let terminal = failure.as_ref().is_some_and(|error| {
                error.chain().any(|cause| {
                    cause
                        .downcast_ref::<NoReplay>()
                        .is_some_and(NoReplay::terminal_marker)
                })
            });
            // A partial event is not proof of an empty response; preserve it unmodified.
            if no_replay || exposed || !unparsed.trim().is_empty() || !transport_failure {
                if !pending.is_empty() { yield pending; }
                if let Some(ref error) = failure {
                    let detail = RequestFailure::stream(error, Some(budget.used()), attempts.status(), exposed)
                        .with_terminal_marker(terminal);
                    crate::performance::retry_events::emit(&budget, "stream_failed", Some(detail.as_json()), None, false, None).await;
                    Err(anyhow::Error::new(detail))?;
                }
                break;
            }
            if attempt + 1 == total || budget.remaining() == 0 {
                if !pending.is_empty() { yield pending; }
                let detail = match &failure {
                    Some(error) => RequestFailure::stream(error, Some(budget.used()), attempts.status(), false)
                        .with_terminal_marker(terminal),
                    None => RequestFailure::stream_exhausted(budget.used(), attempts.status()),
                };
                crate::performance::retry_events::emit(&budget, "stream_failed", Some(detail.as_json()), None, false, None).await;
                Err(anyhow::Error::new(detail))?;
            }
            let delay = crate::performance::retry_events::delay(budget.used(), None);
            let detail = match &failure {
                Some(error) => RequestFailure::stream(error, Some(budget.used()), attempts.status(), false),
                None => RequestFailure::stream_exhausted(budget.used(), attempts.status()),
            };
            crate::performance::retry_events::emit(&budget, "retry_wait", Some(detail.as_json()), Some(delay), false, None).await;
            // Dropping the consumer cancels this delay and the next attempt.
            tokio::time::sleep(delay).await;
            next = Some(send(&mut open, &identity, &budget, &attempts).await?);
        }
    });
    Ok((stream, counter))
}

/// Custom OpenAI-compatible request builder entry.
///
/// Send retries and pre-content stream retries share one attempt budget, so a
/// model request never exceeds `1 + http.max_retries` HTTP attempts in total.
pub(super) async fn response(
    request: reqwest::RequestBuilder,
    identity: RequestIdentity,
    options: super::request_options::RequestOptions,
    provider: super::provider_manager::ResolvedProvider,
    api: &'static str,
) -> Result<(DecodedStream, AttemptCounter)> {
    attempts(
        identity,
        builder_opener(request, options, provider, api),
        |response| Box::pin(super::utf8_stream::response(response)),
    )
    .await
}

/// Same custom request, but the caller owns the attempt budget. A
/// single-attempt budget performs one physical POST even for a retryable
/// status, a header failure or a pre-content body timeout.
pub(super) async fn response_with_budget(
    request: reqwest::RequestBuilder,
    identity: RequestIdentity,
    options: super::request_options::RequestOptions,
    provider: super::provider_manager::ResolvedProvider,
    api: &'static str,
    budget: AttemptBudget,
) -> Result<(DecodedStream, AttemptCounter)> {
    attempts_with_budget(
        identity,
        builder_opener(request, options, provider, api),
        |response| Box::pin(super::utf8_stream::response(response)),
        budget,
    )
    .await
}

fn builder_opener(
    request: reqwest::RequestBuilder,
    options: super::request_options::RequestOptions,
    provider: super::provider_manager::ResolvedProvider,
    api: &'static str,
) -> impl FnMut(&RequestIdentity) -> OpenFuture + Send + 'static {
    move |identity: &RequestIdentity| -> OpenFuture {
        let request = request.try_clone();
        let options = options.clone();
        let provider = provider.clone();
        let headers = identity.headers();
        Box::pin(async move {
            let request =
                request.ok_or_else(|| anyhow::anyhow!("Model request cannot be replayed"))?;
            let response = crate::performance::checked_response(
                super::request_recording::send(
                    request.headers(headers),
                    &options,
                    &provider,
                    api,
                    true,
                )
                .await?,
            )
            .await?;
            Ok(response)
        })
    }
}

pub(super) fn empty_prelude(payload: &str) -> bool {
    serde_json::from_str::<Value>(payload)
        .map(|event| empty_prelude_value(&event))
        .unwrap_or(false)
}

/// The same rules for a parsed event. Decoders classify raw provider data with
/// this before normalization can drop or rewrite it.
pub(super) fn empty_prelude_value(event: &Value) -> bool {
    if !event["error"].is_null()
        || !event["provider_content"].is_null()
        || event["response_content_started"] == true
    {
        return false;
    }
    match event["type"].as_str() {
        Some("ping") => true,
        Some("message_start") => {
            let message = &event["message"];
            message["content"].as_array().is_some_and(Vec::is_empty)
                && message["stop_reason"].is_null()
                && message["usage"]["output_tokens"].as_u64().unwrap_or(0) == 0
        }
        Some("content_block_start") => {
            let block = &event["content_block"];
            matches!(block["type"].as_str(), Some("text" | "thinking"))
                && ["text", "thinking", "signature"]
                    .iter()
                    .all(|key| block[*key].is_null() || block[*key] == "")
        }
        None => event["choices"].as_array().is_some_and(|choices| {
            event["usage"]["completion_tokens"].as_u64().unwrap_or(0) == 0
                && choices.iter().all(|choice| {
                    choice["finish_reason"].is_null()
                        && choice["delta"].as_object().is_some_and(|delta| {
                            delta.iter().all(|(key, value)| {
                                (key == "role" && value == "assistant")
                                    || (matches!(
                                        key.as_str(),
                                        "content" | "reasoning_content" | "reasoning"
                                    ) && (value.is_null() || value == ""))
                            })
                        })
                })
        }),
        _ => false,
    }
}

#[cfg(test)]
#[path = "stream_retry_tests.rs"]
mod tests;
