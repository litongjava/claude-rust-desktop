//! IM threads belong to the transport and can exist without a Code conversation.
use super::Stores;
use crate::im_integration::session_manager::{SessionContext, SessionMessage};
use anyhow::{ensure, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

impl Stores {
    pub fn save_im_thread(
        &self,
        thread: &SessionContext,
        message: Option<&SessionMessage>,
    ) -> Result<()> {
        let key = thread.thread_key();
        let body = self.put_payload(
            &json!({"latest":thread.last_message_content,"metadata":thread.metadata}),
        )?;
        let message_body = message
            .map(|m| {
                ensure!(
                    m.session_key == key,
                    "IM message belongs to a different thread"
                );
                let mut envelope = json!({"content":m.content});
                if let Some(raw) = &m.raw_data {
                    envelope["raw"] = raw.clone();
                }
                self.put_payload(&envelope)
            })
            .transpose()?;
        self.global().write("save IM thread", &key, false, |tx| {
            if let (Some(m), Some(digest)) = (message, &message_body) {
                if let Some(matches) = tx.query_row(
                    "SELECT session_key=?2 AND role=?3 AND content=?4 AND message_type=?5 AND timestamp=?6
                        FROM im_session_messages WHERE id=?1",
                    params![m.id,key,m.role,digest,m.message_type,m.timestamp.to_rfc3339()], |r| r.get::<_,bool>(0)
                ).optional()? {
                    ensure!(matches, "IM message ID already has different content");
                    return Ok(());
                }
            }
            tx.execute("INSERT INTO im_sessions(session_key,platform,user_id,chat_id,thread_id,created_at,updated_at,message_count,conversation_context)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(session_key) DO UPDATE SET
                updated_at=excluded.updated_at, conversation_context=excluded.conversation_context,
                message_count=im_sessions.message_count+?10",
                params![key,thread.platform,thread.user_id,thread.chat_id,thread.thread_id,thread.created_at.to_rfc3339(),
                        thread.updated_at.to_rfc3339(),thread.message_count,body,i64::from(message.is_some())])?;
            if let (Some(m), Some(digest)) = (message, &message_body) {
                tx.execute("INSERT INTO im_session_messages(id,session_key,role,content,message_type,timestamp)
                    VALUES(?1,?2,?3,?4,?5,?6)",params![m.id,key,m.role,digest,m.message_type,m.timestamp.to_rfc3339()])?;
            }
            Ok(())
        })
    }

    pub fn im_threads(&self) -> Result<Vec<SessionContext>> {
        let threads = self.global().read(|c| {
            let mut stmt = c.prepare("SELECT platform,user_id,chat_id,thread_id,created_at,updated_at,message_count,conversation_context FROM im_sessions ORDER BY session_key")?;
            let rows = stmt.query_map([], |r| Ok((SessionContext {
                platform:r.get(0)?,user_id:r.get(1)?,chat_id:r.get(2)?,thread_id:r.get(3)?,
                created_at:r.get(4)?,updated_at:r.get(5)?,message_count:r.get(6)?,
                last_message_content:None,metadata: Value::Null,
            },r.get::<_,String>(7)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        threads
            .into_iter()
            .map(|(mut thread, digest)| {
                let mut body = self.payload(&digest)?;
                thread.last_message_content = serde_json::from_value(body["latest"].take())?;
                thread.metadata = body["metadata"].take();
                Ok(thread)
            })
            .collect()
    }

    pub fn im_messages(&self, thread: &str, limit: usize) -> Result<Vec<SessionMessage>> {
        let messages = self.global().read(|c| {
            let mut q = c.prepare(
                "SELECT id,session_key,role,message_type,timestamp,content FROM im_session_messages
                WHERE session_key=?1 ORDER BY timestamp DESC,id DESC LIMIT ?2",
            )?;
            let rows = q.query_map(
                params![thread, i64::try_from(limit).unwrap_or(i64::MAX)],
                |r| {
                    Ok((
                        SessionMessage {
                            id: r.get(0)?,
                            session_key: r.get(1)?,
                            role: r.get(2)?,
                            message_type: r.get(3)?,
                            timestamp: r.get(4)?,
                            content: String::new(),
                            raw_data: None,
                        },
                        r.get::<_, String>(5)?,
                    ))
                },
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        messages
            .into_iter()
            .map(|(mut message, digest)| {
                let mut body = self.payload(&digest)?;
                message.content = serde_json::from_value(body["content"].take())?;
                message.raw_data = body.as_object_mut().and_then(|obj| obj.remove("raw"));
                Ok(message)
            })
            .collect()
    }

    pub fn delete_im_thread(&self, thread: &str) -> Result<()> {
        self.global().write("delete IM thread", thread, false, |c| {
            c.execute("DELETE FROM im_sessions WHERE session_key=?1", [thread])?;
            Ok(())
        })
    }
}

use crate::im_integration::session_manager::ThreadHistory;

impl ThreadHistory for Stores {
    fn im_threads(&self) -> Result<Vec<SessionContext>> {
        self.im_threads()
    }
    fn save_im_thread(
        &self,
        thread: &SessionContext,
        message: Option<&SessionMessage>,
    ) -> Result<()> {
        self.save_im_thread(thread, message)
    }
    fn im_messages(&self, thread: &str, limit: usize) -> Result<Vec<SessionMessage>> {
        self.im_messages(thread, limit)
    }
    fn delete_im_thread(&self, thread: &str) -> Result<()> {
        self.delete_im_thread(thread)
    }
}

impl ThreadHistory for crate::db::DbManager {
    fn im_threads(&self) -> Result<Vec<SessionContext>> {
        self.with_conn(|c| -> Result<_> {
            c.execute_batch("CREATE TABLE IF NOT EXISTS im_session_messages(id TEXT PRIMARY KEY, session_key TEXT NOT NULL REFERENCES im_sessions(session_key) ON DELETE CASCADE,
                role TEXT NOT NULL,content TEXT NOT NULL,message_type TEXT NOT NULL,timestamp TEXT NOT NULL,raw_data TEXT);
                CREATE INDEX IF NOT EXISTS idx_im_session_messages_session ON im_session_messages(session_key);
                CREATE INDEX IF NOT EXISTS idx_im_session_messages_timestamp ON im_session_messages(timestamp);")?;
            let mut q=c.prepare("SELECT platform,user_id,chat_id,thread_id,created_at,updated_at,message_count,last_message_content,metadata FROM im_sessions")?;
            let rows=q.query_map([],|r| Ok(SessionContext{platform:r.get(0)?,user_id:r.get(1)?,chat_id:r.get(2)?,thread_id:r.get(3)?,created_at:r.get(4)?,
                updated_at:r.get(5)?,message_count:r.get(6)?,last_message_content:r.get(7)?,metadata:r.get::<_,Option<String>>(8)?.and_then(|s|serde_json::from_str(&s).ok()).unwrap_or_else(||json!({}))}))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })?
    }

    fn save_im_thread(
        &self,
        thread: &SessionContext,
        message: Option<&SessionMessage>,
    ) -> Result<()> {
        let key = thread.thread_key();
        self.with_conn(|c| crate::db::write_tx(c,"save IM thread",super::WRITE_DEADLINE,|tx|{
            tx.execute("INSERT INTO im_sessions(session_key,platform,user_id,chat_id,thread_id,created_at,updated_at,message_count,last_message_content,metadata)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(session_key) DO UPDATE SET updated_at=excluded.updated_at,
                last_message_content=excluded.last_message_content,metadata=excluded.metadata,message_count=im_sessions.message_count+?11",
                params![key,thread.platform,thread.user_id,thread.chat_id,thread.thread_id,thread.created_at.to_rfc3339(),thread.updated_at.to_rfc3339(),thread.message_count,thread.last_message_content,thread.metadata.to_string(),i64::from(message.is_some())])?;
            if let Some(m)=message {
                ensure!(m.session_key==key,"IM message belongs to a different thread");
                tx.execute("INSERT INTO im_session_messages(id,session_key,role,content,message_type,timestamp,raw_data) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![m.id,key,m.role,m.content,m.message_type,m.timestamp.to_rfc3339(),m.raw_data.as_ref().map(Value::to_string)])?;
            }
            Ok(())
        }))?
    }
    fn im_messages(&self, key: &str, limit: usize) -> Result<Vec<SessionMessage>> {
        self.with_conn(|c| -> Result<_> {
            let mut q=c.prepare("SELECT id,session_key,role,content,message_type,timestamp,raw_data FROM im_session_messages WHERE session_key=?1 ORDER BY timestamp DESC,id DESC LIMIT ?2")?;
            let rows=q.query_map(params![key,i64::try_from(limit).unwrap_or(i64::MAX)],|r| Ok(SessionMessage{
                id:r.get(0)?,session_key:r.get(1)?,role:r.get(2)?,content:r.get(3)?,message_type:r.get(4)?,timestamp:r.get(5)?,
                raw_data:r.get::<_,Option<String>>(6)?.and_then(|s|serde_json::from_str(&s).ok())
            }))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })?
    }
    fn delete_im_thread(&self, key: &str) -> Result<()> {
        self.with_conn(|c| c.execute("DELETE FROM im_sessions WHERE session_key=?1", [key]))??;
        Ok(())
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Clone)]
pub struct FeishuChatMapping {
    pub chat_id: String,
    pub conversation_id: String,
    pub title: String,
    pub created_at: String,
    pub last_active_at: String,
    pub message_count: u64,
}


fn feishu_row(r:&rusqlite::Row<'_>)->rusqlite::Result<FeishuChatMapping>{
    Ok(FeishuChatMapping{ chat_id:r.get(0)?,conversation_id:r.get(1)?,title:r.get(2)?,created_at:r.get(3)?,last_active_at:r.get(4)?,message_count:r.get(5)? })
}
impl Stores {
    pub(super) fn feishu_mapping(&self,chat:&str)->Result<Option<FeishuChatMapping>> {
        self.global().read(|c|Ok(c.query_row("SELECT chat_id,conversation_id,title,created_at,last_active_at,message_count FROM feishu_chat_mappings WHERE chat_id=?1",[chat],feishu_row).optional()?))
    }
    pub fn feishu_mappings(&self)->Result<Vec<FeishuChatMapping>> {
        self.global().read(|c| {
            let mut q=c.prepare("SELECT chat_id,conversation_id,title,created_at,last_active_at,message_count FROM feishu_chat_mappings ORDER BY last_active_at DESC")?;
            let rows=q.query_map([],feishu_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
    pub fn delete_feishu_mapping(&self,chat:&str)->Result<()> {
        self.global().write("forget Feishu routing",chat,false,|c| {c.execute("DELETE FROM feishu_chat_mappings WHERE chat_id=?1",[chat])?; Ok(())})
    }
}
