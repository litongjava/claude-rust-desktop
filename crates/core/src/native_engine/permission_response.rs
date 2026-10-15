//! Host permission decisions never travel through ordinary user-answer strings.
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{oneshot, Mutex};

pub type Commit = Box<dyn FnOnce() -> Result<()> + Send>;

pub enum PermissionResponse {
    Allow {
        updated_input: Option<Value>,
        /// A prevalidated host policy patch, published only after effective input validation.
        commit: Option<Commit>,
    },
    Deny {
        message: String,
        interrupt: bool,
    },
}

pub type Mailbox = Arc<Mutex<HashMap<String, oneshot::Sender<PermissionResponse>>>>;

pub async fn respond(
    mailbox: &Mailbox,
    request_id: &str,
    response: PermissionResponse,
) -> Result<()> {
    let sender = mailbox
        .lock()
        .await
        .remove(request_id)
        .ok_or_else(|| anyhow!("Unknown or completed tool permission request: {request_id}"))?;
    sender
        .send(response)
        .map_err(|_| anyhow!("Tool permission request is no longer active"))
}

pub(crate) struct Pending {
    pub mailbox: Mailbox,
    pub request_id: String,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if let Ok(mut entries) = self.mailbox.try_lock() {
            entries.remove(&self.request_id);
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let mailbox = self.mailbox.clone();
            let id = self.request_id.clone();
            runtime.spawn(async move {
                mailbox.lock().await.remove(&id);
            });
        }
    }
}
