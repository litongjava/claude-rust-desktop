use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::message_router::UnifiedMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContext {
    pub platform: String,
    pub user_id: String,
    pub chat_id: String,
    pub thread_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub message_count: u64,
    pub last_message_content: Option<String>,
    pub metadata: serde_json::Value,
}

impl SessionContext {
    pub fn session_key(&self) -> String {
        format!("{}:{}:{}", self.platform, self.user_id, self.chat_id)
    }

    pub fn thread_key(&self) -> String {
        match &self.thread_id {
            Some(thread_id) => {
                format!(
                    "{}:{}:{}:{}",
                    self.platform, self.user_id, self.chat_id, thread_id
                )
            }
            None => self.session_key(),
        }
    }
}

impl From<&UnifiedMessage> for SessionContext {
    fn from(msg: &UnifiedMessage) -> Self {
        Self {
            platform: msg.platform.clone(),
            user_id: msg.user_id.clone(),
            chat_id: msg.chat_id.clone(),
            thread_id: msg.thread_id.clone(),
            created_at: msg.timestamp,
            updated_at: msg.timestamp,
            message_count: 1,
            last_message_content: Some(msg.content.clone()),
            metadata: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    pub id: String,
    pub session_key: String,
    pub role: String,
    pub content: String,
    pub message_type: String,
    pub timestamp: DateTime<Utc>,
    pub raw_data: Option<serde_json::Value>,
}

pub trait ThreadHistory: Send + Sync + 'static {
    fn im_threads(&self) -> Result<Vec<SessionContext>>;
    fn save_im_thread(
        &self,
        thread: &SessionContext,
        message: Option<&SessionMessage>,
    ) -> Result<()>;
    fn im_messages(&self, thread: &str, limit: usize) -> Result<Vec<SessionMessage>>;
    fn delete_im_thread(&self, thread: &str) -> Result<()>;
}

pub struct SessionManager {
    db: Arc<dyn ThreadHistory>,
    sessions: RwLock<HashMap<String, SessionContext>>,
}

impl SessionManager {
    pub fn new(db: Arc<dyn ThreadHistory>) -> Self {
        Self {
            db,
            sessions: Default::default(),
        }
    }

    pub async fn initialize(&self) -> Result<()> {
        let db = self.db.clone();
        let mut sessions = self.sessions.write().await;
        let rows = tokio::task::spawn_blocking(move || db.im_threads()).await??;
        *sessions = rows.into_iter().map(|s| (s.thread_key(), s)).collect();
        Ok(())
    }

    async fn save(&self, thread: SessionContext, message: Option<SessionMessage>) -> Result<()> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || db.save_im_thread(&thread, message.as_ref())).await?
    }

    pub async fn get_or_create_session(&self, msg: &UnifiedMessage) -> Result<SessionContext> {
        let mut sessions = self.sessions.write().await;
        let key = msg.thread_key();
        if let Some(s) = sessions.get(&key) {
            return Ok(s.clone());
        }
        let thread = SessionContext::from(msg);
        self.save(thread.clone(), None).await?;
        sessions.insert(key, thread.clone());
        Ok(thread)
    }
    pub async fn update_session(&self, msg: &UnifiedMessage) -> Result<SessionContext> {
        let mut sessions = self.sessions.write().await;
        let key = msg.thread_key();
        let mut thread = sessions
            .get(&key)
            .cloned()
            .unwrap_or_else(|| SessionContext::from(msg));
        thread.updated_at = Utc::now();
        thread.last_message_content = Some(msg.content.clone());
        thread.message_count += 1;
        let message = SessionMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_key: key.clone(),
            role: "user".into(),
            content: msg.content.clone(),
            message_type: msg.message_type.as_str().into(),
            timestamp: msg.timestamp,
            raw_data: Some(msg.raw_data.clone()),
        };
        self.save(thread.clone(), Some(message)).await?;
        sessions.insert(key, thread.clone());
        Ok(thread)
    }

    pub async fn get_session(
        &self,
        platform: &str,
        user_id: &str,
        chat_id: &str,
        thread_id: Option<&str>,
    ) -> Option<SessionContext> {
        self.sessions
            .read()
            .await
            .get(&thread_key(platform, user_id, chat_id, thread_id))
            .cloned()
    }
    pub async fn list_sessions(&self) -> Vec<SessionContext> {
        self.sessions.read().await.values().cloned().collect()
    }
    pub async fn list_sessions_by_platform(&self, platform: &str) -> Vec<SessionContext> {
        self.sessions
            .read()
            .await
            .values()
            .filter(|s| s.platform == platform)
            .cloned()
            .collect()
    }
    pub async fn delete_session(
        &self,
        platform: &str,
        user_id: &str,
        chat_id: &str,
        thread_id: Option<&str>,
    ) -> Result<()> {
        let key = thread_key(platform, user_id, chat_id, thread_id);
        let db = self.db.clone();
        let mut sessions = self.sessions.write().await;
        let id = key.clone();
        tokio::task::spawn_blocking(move || db.delete_im_thread(&id)).await??;
        sessions.remove(&key);
        Ok(())
    }
    pub async fn get_session_messages(
        &self,
        platform: &str,
        user_id: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionMessage>> {
        let key = thread_key(platform, user_id, chat_id, thread_id);
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || db.im_messages(&key, limit)).await?
    }
    pub async fn add_message_to_session(
        &self,
        platform: &str,
        user_id: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        role: &str,
        content: &str,
        message_type: &str,
        raw_data: Option<serde_json::Value>,
    ) -> Result<SessionMessage> {
        let key = thread_key(platform, user_id, chat_id, thread_id);
        let mut sessions = self.sessions.write().await;
        let mut thread = sessions
            .get(&key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("IM thread unavailable"))?;
        let message = SessionMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_key: key.clone(),
            role: role.into(),
            content: content.into(),
            message_type: message_type.into(),
            timestamp: Utc::now(),
            raw_data,
        };
        thread.message_count += 1;
        thread.updated_at = message.timestamp;
        thread.last_message_content = Some(content.to_owned());
        self.save(thread.clone(), Some(message.clone())).await?;
        sessions.insert(key, thread);
        Ok(message)
    }
    pub async fn set_session_metadata(
        &self,
        platform: &str,
        user_id: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        metadata: serde_json::Value,
    ) -> Result<()> {
        let key = thread_key(platform, user_id, chat_id, thread_id);
        let mut sessions = self.sessions.write().await;
        if let Some(current) = sessions.get_mut(&key) {
            let mut next = current.clone();
            next.metadata = metadata;
            next.updated_at = Utc::now();
            self.save(next.clone(), None).await?;
            *current = next;
        }
        Ok(())
    }
}

fn thread_key(platform: &str, user: &str, chat: &str, thread: Option<&str>) -> String {
    match thread {
        Some(t) => format!("{platform}:{user}:{chat}:{t}"),
        None => format!("{platform}:{user}:{chat}"),
    }
}
