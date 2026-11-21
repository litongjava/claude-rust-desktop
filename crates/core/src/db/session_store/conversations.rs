use super::{Branch, CreateOptions, Stores};
use crate::db::{conversation_repo as conversations, message_repo as messages, transcript};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::Value;

pub enum Rewind<'a> {
    From(&'a str),
    Tail(i64),
}

fn validate_chat(c: &rusqlite::Connection, id: &str) -> Result<()> {
    if let Some(row) = conversations::conversation_metadata(c, id)? {
        anyhow::ensure!(
            row.workspace_mode != "code"
                || row.project_id.as_deref().is_some_and(|s| !s.is_empty()),
            "Select a project before sending messages in Code mode"
        );
        crate::native_engine::workspace::conversation_project(c, id)?;
    }
    Ok(())
}

impl Stores {
    pub fn validate_chat_project(&self, id: &str) -> Result<()> {
        self.global().read(|c| {
            let tx = c.unchecked_transaction()?;
            validate_chat(&tx, id)
        })
    }
    pub fn admit_chat_message(&self, id: &str, model: &str, content: Option<&Value>) -> Result<()> {
        let content = content.filter(|v| v.as_str() != Some(""));
        let row = self.global().read_directory_row(id, |c| {
            let tx = c.unchecked_transaction()?;
            validate_chat(&tx, id)?;
            super::directory::one(&tx, id)
        })?;
        match row {
            Some(entry) => {
                let shard = self.open_listed(&entry)?;
                if let Some(content) = content {
                    let message = uuid::Uuid::new_v4().to_string();
                    shard.write(|w| w.append_user(&message, content))?;
                }
            }
            None => {
                let first_message = content
                    .map(|value| -> Result<_> {
                        let (text, normalized) = messages::prepare_user_message(value)?;
                        Ok(super::FirstMessage {
                            text,
                            typed: normalized.is_array().then_some(normalized),
                        })
                    })
                    .transpose()?;
                self.create(
                    id,
                    CreateOptions {
                        model: Some(model.into()),
                        first_message,
                        ..Default::default()
                    },
                )?;
            }
        }
        Ok(())
    }

    pub fn create_conversation(
        &self,
        id: &str,
        title: Option<String>,
        model: Option<String>,
        research: bool,
        mode: conversations::WorkspaceMode,
    ) -> Result<()> {
        self.create(
            id,
            CreateOptions {
                title,
                model,
                research,
                workspace_mode: Some(mode.as_str().into()),
                ..Default::default()
            },
        )?;
        Ok(())
    }

    pub fn conversation_detail(
        &self,
        id: &str,
    ) -> Result<(Option<conversations::ConversationRow>, Vec<Value>)> {
        let Some(row) = self
            .global()
            .read_directory_row(id, |c| super::directory::one(c, id))?
        else {
            return Ok((None, Vec::new()));
        };
        let messages = self.open_listed(&row)?.read(|r| r.enriched_messages())?;
        Ok((Some(row.conversation), messages))
    }

    pub fn conversation_metadata(
        &self,
        id: &str,
    ) -> Result<Option<conversations::ConversationRow>> {
        self.global().conversation(id)
    }

    pub fn rewind_messages(&self, id: &str, boundary: Rewind<'_>) -> Result<Vec<Value>> {
        let session = self.open_session(id)?;
        session.write(|w| {
            let first=match boundary {
                Rewind::From(m)=>w.0.query_row("SELECT id FROM messages WHERE id=?1",[m],|r|r.get::<_,String>(0)).optional()?,
                Rewind::Tail(n)=>w.0.query_row("SELECT id FROM (SELECT id,sort_order FROM messages ORDER BY sort_order DESC,rowid DESC LIMIT ?1) ORDER BY sort_order LIMIT 1",[n],|r|r.get::<_,String>(0)).optional()?,
            };
            if let Some(m)=first {w.rewind_from(&m)?;}
            transcript::enriched_messages(w.0,id)
        })
    }

    pub fn branch_conversation(
        &self,
        id: &str,
        message: Option<&str>,
        index: Option<usize>,
    ) -> Result<String> {
        let destination = uuid::Uuid::new_v4().to_string();
        let boundary = if message.is_none() && index.is_none() {
            Branch::Full
        } else {
            Branch::Through { message, index }
        };
        self.open_session(id)?.branch(&destination, boundary)?;
        Ok(destination)
    }
}
impl crate::db::DbManager {
    pub fn validate_chat_project(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            validate_chat(&tx, id)
        })?
    }
    pub fn admit_chat_message(&self, id: &str, model: &str, content: Option<&Value>) -> Result<()> {
        self.with_conn(|c|crate::db::write_tx(c,"admit chat user message",super::WRITE_DEADLINE,|tx|{
            validate_chat(tx,id)?;
            let now=chrono::Utc::now().to_rfc3339();
            if conversations::conversation_metadata(tx,id)?.is_none(){
                conversations::insert_conversation(tx,id,None,Some(model),None,None,None,false,false,false,&now,&now,0)?;
            }
            if let Some(content)=content.filter(|v|v.as_str()!=Some("")) {
                let order=tx.query_row("SELECT coalesce(max(sort_order),-1)+1 FROM messages WHERE conversation_id=?1",[id],|r|r.get(0))?;
                messages::save_user_message(tx,&uuid::Uuid::new_v4().to_string(),id,content,&now,order)?;
            }
            tx.execute("UPDATE conversations SET message_count=(SELECT count(*) FROM messages WHERE conversation_id=?1),updated_at=?2 WHERE id=?1",[id,&now])?;
            Ok(())
        }))?
    }

    pub fn create_conversation(
        &self,
        id: &str,
        title: Option<String>,
        model: Option<String>,
        research: bool,
        mode: conversations::WorkspaceMode,
    ) -> Result<()> {
        self.with_conn(|c| {
            crate::db::write_tx(c, "create conversation", super::WRITE_DEADLINE, |tx| {
                let now = chrono::Utc::now().to_rfc3339();
                conversations::insert_conversation(
                    tx,
                    id,
                    title.as_deref(),
                    model.as_deref(),
                    None,
                    None,
                    None,
                    research,
                    false,
                    false,
                    &now,
                    &now,
                    0,
                )?;
                conversations::set_workspace_mode(tx, id, mode.as_str())
            })
        })?
    }
    pub fn conversation_detail(
        &self,
        id: &str,
    ) -> Result<(Option<conversations::ConversationRow>, Vec<Value>)> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            Ok((
                conversations::get_conversation(&tx, id)?,
                transcript::enriched_messages(&tx, id)?,
            ))
        })?
    }
    pub fn conversation_metadata(
        &self,
        id: &str,
    ) -> Result<Option<conversations::ConversationRow>> {
        self.with_conn(|c| conversations::get_conversation(c, id))?
    }
    pub fn rewind_messages(&self, id: &str, boundary: Rewind<'_>) -> Result<Vec<Value>> {
        self.with_conn(|c| {
            crate::db::write_tx(c, "rewind conversation", super::WRITE_DEADLINE, |tx| {
                match boundary {
                    Rewind::From(mid) => {
                        if let Some(m) =
                            messages::get_message(tx, mid)?.filter(|m| m.conversation_id == id)
                        {
                            messages::delete_messages_from(tx, id, m.sort_order)?;
                        }
                    }
                    Rewind::Tail(count) => messages::delete_messages_tail(tx, id, count)?,
                }
                transcript::enriched_messages(tx, id)
            })
        })?
    }
    pub fn branch_conversation(
        &self,
        id: &str,
        message: Option<&str>,
        index: Option<usize>,
    ) -> Result<String> {
        self.with_conn(|c| conversations::branch_conversation(c, id, message, index))?
    }
}
