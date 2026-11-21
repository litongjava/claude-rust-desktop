//! G decides a combined edit after S has validated the full replacement. The
//! redo record stays beside S until its transaction commits durably. Every
//! subsequent key checkout finishes an interrupted decision before admitting a reader.
use super::{layout, state, SessionDb, Stores, WRITE_DEADLINE};
use anyhow::{ensure, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs, path::Path};

pub(super) const PENDING: &str = "pending-conversation-edit.json";
const RECEIPT: &str = "conversation.edit-receipt";

#[derive(Serialize, Deserialize)]
struct PendingEdit<'a> {
    id: std::borrow::Cow<'a, str>,
    incarnation: &'a str,
    operation: &'a str,
    messages: std::borrow::Cow<'a, [Value]>,
}

impl SessionDb {
    pub(super) fn edit_history_and_metadata(&self, messages: &[Value], body: &Value) -> Result<()> {
        let operation = uuid::Uuid::new_v4().to_string();
        let dir = self.stores.sessions().join(&self.key);
        let path = dir.join(PENDING);
        let mut file = tempfile::NamedTempFile::new_in(&dir)?;
        serde_json::to_writer(
            &mut file,
            &PendingEdit {
                id: self.id().into(),
                incarnation: self.incarnation(),
                operation: &operation,
                messages: messages.into(),
            },
        )?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&path)?;
        layout::sync_dir(&dir)?;
        self.write_locked_with_commit(
            "replace conversation history",
            WRITE_DEADLINE,
            true,
            |w| {
                w.replace_messages(messages)?;
                state::set_local(w.0, RECEIPT, &operation)
            },
            |tx| {
                self.stores.global().write("edit conversation", self.id(), true, |g| {
                    let decided: Option<String> = g.query_row(
                        "SELECT operation FROM conversation_edit_decisions WHERE incarnation=?1",
                        [self.incarnation()], |r| r.get(0)
                    ).optional()?;
                    if decided.as_deref() != Some(&operation) {
                        super::conversations::edit_metadata(g, self.id(), body)?;
                        g.execute(
                            "INSERT INTO conversation_edit_decisions(incarnation,operation) VALUES(?1,?2)
                             ON CONFLICT(incarnation) DO UPDATE SET operation=excluded.operation",
                            [self.incarnation(), &operation],
                        )?;
                    }
                    self.admit_edited_workspace(g, body)
                })?;
                self.stores.step(super::Phase::EditMetadata)?;
                Ok(tx.commit()?)
            },
        )?;
        fs::remove_file(&path)?;
        layout::sync_dir(&dir)
    }
}

impl Stores {
    /// Caller holds the exclusive lifecycle lock. A staged file alone carries no authority.
    pub(super) fn recover_conversation_edit(&self, key: &str, path: &Path) -> Result<()> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let edit: PendingEdit = serde_json::from_slice(&bytes)?;
        ensure!(
            layout::storage_key(&edit.id)? == key,
            "Edit journal belongs to another session"
        );
        let committed: bool = self.global().read(|g| {
            Ok(g.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversation_edit_decisions e JOIN conversations c USING(incarnation)
                 WHERE c.id=?1 AND e.incarnation=?2 AND e.operation=?3)",
                [edit.id.as_ref(), edit.incarnation, edit.operation],
                |r| r.get(0),
            )?)
        })?;
        if committed {
            let shard = SessionDb {
                stores: self.clone(),
                id: edit.id.into(),
                incarnation: edit.incarnation.into(),
                key: key.into(),
            };
            shard.write_locked_with_commit(
                "recover conversation edit",
                WRITE_DEADLINE,
                true,
                |w| {
                    if state::local(w.0, RECEIPT)?.as_deref() != Some(edit.operation) {
                        w.replace_messages(&edit.messages)?;
                        state::set_local(w.0, RECEIPT, edit.operation)?;
                    }
                    Ok(())
                },
                |tx| Ok(tx.commit()?),
            )?;
        }
        fs::remove_file(path)?;
        layout::sync_dir(path.parent().expect("session directory"))
    }
}
