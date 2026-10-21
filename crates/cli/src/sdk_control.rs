//! Independent typed SDK control brokers with cancellation-safe request ownership.
use anyhow::{anyhow, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};
use tokio::sync::oneshot;
#[derive(Default)]
struct Pending {
    requests: HashMap<String, oneshot::Sender<Result<Value>>>,
    cancelled: VecDeque<String>,
}
#[derive(Default)]
pub struct Broker {
    pending: Mutex<Pending>,
    closed: AtomicBool,
}
impl Broker {
    pub fn owns(&self, id: &str) -> bool {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        pending.requests.contains_key(id) || pending.cancelled.iter().any(|entry| entry == id)
    }
    pub fn respond(&self, frame: &Value) -> Result<()> {
        let response = &frame["response"];
        let id = response["request_id"]
            .as_str()
            .context("Control response is missing request_id")?;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(sender) = pending.requests.remove(id) {
            let result = if response["subtype"] == "success" {
                response
                    .get("response")
                    .cloned()
                    .context("Missing SDK control response output")
            } else {
                Err(anyhow!(
                    "SDK control request rejected: {}",
                    response["error"]
                ))
            };
            let _ = sender.send(result);
            Ok(())
        } else if pending.cancelled.iter().any(|entry| entry == id) {
            Ok(())
        } else {
            Err(anyhow!("Unknown or completed SDK control request: {id}"))
        }
    }
    fn cancel(&self, id: &str) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(sender) = pending.requests.remove(id) {
            pending.cancelled.push_back(id.into());
            while pending.cancelled.len() > 512 {
                pending.cancelled.pop_front();
            }
            let _ = sender.send(Err(anyhow!("SDK control request cancelled")));
            let _ = crate::session::emit_json(
                &json!({"type":"control_cancel_request","request_id":id}),
            );
        }
    }
    pub fn cancel_all(&self) {
        let ids = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .requests
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in ids {
            self.cancel(&id);
        }
    }
    pub fn has_pending(&self) -> bool {
        !self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .requests
            .is_empty()
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.cancel_all();
    }
}
struct Request<'a> {
    broker: &'a Broker,
    id: String,
}
impl Drop for Request<'_> {
    fn drop(&mut self) {
        self.broker.cancel(&self.id);
    }
}

impl Broker {
    pub async fn request(&self, request: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let (send, receive) = oneshot::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            ensure!(!self.is_closed(), "SDK input closed before control request");
            pending.requests.insert(id.clone(), send);
        }
        let _request = Request {
            broker: self,
            id: id.clone(),
        };
        crate::session::emit_json(
            &json!({"type":"control_request","request_id":id,"request":request}),
        )?;
        receive
            .await
            .context("SDK control response channel closed")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn typed_broker_accepts_only_pending_or_actually_cancelled_ids() {
        let broker = Broker::default();
        let (tx, rx) = oneshot::channel();
        broker
            .pending
            .lock()
            .unwrap()
            .requests
            .insert("completed".into(), tx);
        let response =
            json!({"response":{"request_id":"completed","subtype":"success","response":{}}});
        broker.respond(&response).unwrap();
        assert_eq!(rx.await.unwrap().unwrap(), json!({}));
        assert!(broker.respond(&response).is_err());
        assert!(broker
            .respond(
                &json!({"response":{"request_id":"unknown","subtype":"success","response":{}}})
            )
            .is_err());
        let (tx, rx) = oneshot::channel();
        broker
            .pending
            .lock()
            .unwrap()
            .requests
            .insert("cancelled".into(), tx);
        broker.cancel("cancelled");
        broker.cancel("cancelled");
        assert!(rx.await.unwrap().is_err());
        broker.respond(&json!({"response":{"request_id":"cancelled","subtype":"success","response":{"hookSpecificOutput":{"permissionDecision":"allow"}}}})).unwrap();
        assert_eq!(broker.pending.lock().unwrap().cancelled.len(), 1);
    }
}
