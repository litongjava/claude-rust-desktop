//! Every new context window is sealed in the transaction that inserts its first row.
use super::*;
use crate::services::memory_files::prompt::Admission;

pub(crate) enum Kind {
    Compaction,
    ApprovedPlan,
}

pub(crate) enum Storage<'a> {
    Shard(&'a Admission),
    Shared,
}

pub(crate) fn create(
    tx: &Connection,
    session: &str,
    order: i64,
    content: &str,
    kind: Kind,
    storage: Storage<'_>,
) -> Result<String> {
    let id = match storage {
        Storage::Shard(_) => crate::db::transcript::MessageIds::for_shard(tx)?.issue(),
        Storage::Shared => uuid::Uuid::new_v4().to_string(),
    };
    let origin = match kind {
        Kind::Compaction => "compaction",
        Kind::ApprovedPlan => "approved_plan",
    };
    crate::db::message_repo::insert_message(
        tx,
        &id,
        session,
        "user",
        content,
        None,
        &chrono::Utc::now().to_rfc3339(),
        true,
        order,
    )?;
    let metadata = serde_json::json!({"promptOrigin":origin}).to_string();
    match storage {
        Storage::Shard(memory) => {
            tx.execute(
                "INSERT INTO message_metadata VALUES(?1,?2)",
                [&id, &metadata],
            )?;
            memory.freeze(tx, Some(&id))?;
            crate::services::memory_files::prompt::supersede_copied_indexes(tx, order)?;
        }
        Storage::Shared => {
            // The compatibility database has no file-memory scope or metadata table.
            super::state::set_local(tx, &format!("message-origin:{id}"), &metadata)?;
            crate::services::memory_files::prompt::freeze_empty_boundary(tx, &id)?;
        }
    }
    Ok(id)
}

impl SessionDb {
    pub(crate) fn plan_boundary_context(
        &self,
        id: &str,
    ) -> Result<(serde_json::Value, Option<String>)> {
        self.read(|r| {
            let message = crate::db::message_repo::get_message(r.0, id)?
                .context("Approved plan boundary disappeared")?;
            let origin = super::message_origin(r.0, id)?;
            ensure!(
                message.conversation_id == self.id()
                    && message.is_compact_boundary
                    && origin.as_deref() == Some("approved_plan"),
                "Invalid approved plan boundary"
            );
            let prefix = crate::services::memory_files::prompt::frozen_index(r.0, Some(id))?;
            let message = serde_json::json!({
                "id": id, "role": message.role, "content": message.content, "origin": origin
            });
            Ok((message, prefix))
        })
    }
}
