//! Request-scoped completion notifications for native permission dialogs.
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::watch;

#[derive(Default)]
pub(super) struct PermissionWatches {
    pending: HashMap<String, String>,
    completions: FuturesUnordered<BoxFuture<'static, String>>,
}

impl PermissionWatches {
    /// Returns false when the hook already claimed the request before presentation.
    pub fn register(&mut self, id: &str, tool_id: &str, mut closed: watch::Receiver<bool>) -> bool {
        if *closed.borrow() {
            return false;
        }
        self.pending.insert(id.into(), tool_id.into());
        let id = id.to_owned();
        self.completions.push(Box::pin(async move {
            loop {
                let finished = *closed.borrow_and_update();
                if finished {
                    break;
                }
                if closed.changed().await.is_err() {
                    break;
                }
            }
            id
        }));
        true
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub async fn next(&mut self, conversation: &str) -> Value {
        let id = self
            .completions
            .next()
            .await
            .expect("pending permission watch");
        let tool_id = self.pending.remove(&id).unwrap_or_default();
        resolved(conversation, &id, &tool_id)
    }

    /// End-of-stream cancellation also dismisses any outstanding host UI.
    pub fn finish(&mut self, conversation: &str) -> Vec<Value> {
        self.completions.clear();
        self.pending
            .drain()
            .map(|(id, tool)| resolved(conversation, &id, &tool))
            .collect()
    }
}

pub(super) fn resolved(conversation: &str, id: &str, tool: &str) -> Value {
    json!({"type":"tool_permission_resolved","conversation_id":conversation,"request_id":id,"tool_use_id":tool})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn completion_is_request_scoped_and_stream_end_dismisses_only_remaining() {
        let mut watches = PermissionWatches::default();
        let (first, first_rx) = watch::channel(false);
        let (_second, second_rx) = watch::channel(false);
        assert!(watches.register("first", "tool-a", first_rx));
        assert!(watches.register("second", "tool-b", second_rx));
        first.send_replace(true);
        assert_eq!(
            watches.next("conversation").await,
            resolved("conversation", "first", "tool-a")
        );
        assert_eq!(
            watches.finish("conversation"),
            vec![resolved("conversation", "second", "tool-b")]
        );
        assert!(watches.is_empty());
    }

    #[tokio::test]
    async fn already_resolved_does_not_open_and_dropped_sender_cancels() {
        let mut watches = PermissionWatches::default();
        assert!(!watches.register("done", "tool", watch::channel(true).1));
        let (sender, receiver) = watch::channel(false);
        assert!(watches.register("cancelled", "tool", receiver));
        drop(sender);
        assert_eq!(
            watches.next("conversation").await["request_id"],
            "cancelled"
        );
        assert!(watches.is_empty());
    }
}
