use super::*;
use crate::services::{compaction, compaction_requests};
use rusqlite::params;

/// Only digests of provider configuration and fragment inputs reach SQLite.
pub struct CheckpointKey {
    fingerprint: String,
    request: String,
}
impl CheckpointKey {
    pub fn new(
        provider: &crate::native_engine::provider_manager::ResolvedProvider,
        configuration_system: &str,
        request_system: &str,
        fragment: &str,
        output: u32,
    ) -> Self {
        Self {
            fingerprint: compaction::checkpoint::config_fingerprint(provider, configuration_system),
            request: compaction::checkpoint::Checkpoint::request_key(
                request_system,
                fragment,
                output,
            ),
        }
    }
}

pub struct CompactionClaim {
    id: String,
    incarnation: String,
    fingerprint: String,
    pub keep: String,
    pub transcript: String,
}

enum PendingClaim {
    Empty,
    Ready(CompactionClaim),
    Obsolete {
        id: String,
        revision: i64,
        reason: &'static str,
    },
}

impl CompactionClaim {
    pub fn request_id(&self) -> &str {
        &self.id
    }
}

impl SessionWrite<'_> {
    pub fn request_compaction(&self, keep: &str, used_tokens: u64, window: u64) -> Result<()> {
        compaction_requests::validate(self.0, self.1, keep, used_tokens, window)?;
        self.0.execute(
            "INSERT INTO compaction_request(singleton,id,keep,target_message) VALUES(1,?1,?2,(SELECT id FROM messages ORDER BY sort_order DESC,rowid DESC LIMIT 1))
            ON CONFLICT(singleton) DO UPDATE SET id=excluded.id,keep=excluded.keep,target_message=excluded.target_message",
            params![uuid::Uuid::new_v4().to_string(), keep],
        )?;
        Ok(())
    }

    pub fn save_compaction_checkpoint(&self, key: &CheckpointKey, summary: &str) -> Result<()> {
        ensure!(
            !summary.trim().is_empty() && summary.chars().count() <= 32_000,
            "Empty or oversized compaction summary"
        );
        self.0.execute("INSERT INTO compaction_checkpoint VALUES(?1,?2,?3)
            ON CONFLICT(fingerprint,request_key) DO UPDATE SET summary=excluded.summary WHERE summary<>excluded.summary",
            [key.fingerprint.as_str(),key.request.as_str(),summary])?;
        Ok(())
    }
}

impl SessionRead<'_> {
    pub fn compaction_cancellation(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row(
                "SELECT reason FROM compaction_cancellations WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn compaction_checkpoint(&self, key: &CheckpointKey) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row(
                "SELECT summary FROM compaction_checkpoint WHERE fingerprint=?1 AND request_key=?2",
                [&key.fingerprint, &key.request],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn native_compaction_boundary(&self, message: &str) -> Result<Option<serde_json::Value>> {
        let raw: Option<String> = self
            .0
            .query_row(
                "SELECT value FROM native_boundaries WHERE message_id=?1",
                [message],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }
}

impl DeliveryOwner {
    fn check_compaction_session(&self) -> Result<()> {
        let s = &self.session;
        let archived: bool = s.stores.global().read(|c| {
            Ok(c.query_row(
                "SELECT archived FROM conversations WHERE id=?1 AND incarnation=?2",
                [&s.id, &s.incarnation],
                |r| r.get(0),
            )?)
        })?;
        ensure!(!archived, "Session is archived");
        Ok(())
    }

    /// The execution lease is the claim. Leave the request durable until the
    /// boundary commits so a replacement worker can resume the same work.
    pub fn claim_compaction(&self) -> Result<Option<CompactionClaim>> {
        layout::allow_entry()?;
        let s = &self.session;
        self.check_compaction_session()?;
        let candidate = s.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
            let tx = c.transaction()?;
            let request = tx
                .query_row("SELECT q.id,q.keep,EXISTS(SELECT 1 FROM messages WHERE id=q.target_message),m.content_revision
                    FROM compaction_request q JOIN manifest m USING(singleton)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get::<_, bool>(2)?, r.get::<_, i64>(3)?))
                })
                .optional()?;
            let Some((id, keep, has_target, revision)) = request else {
                return Ok(PendingClaim::Empty);
            };
            if !has_target {
                return Ok(PendingClaim::Obsolete {id, revision, reason:"missing compaction target"});
            }
            let (fingerprint, transcript, count) = compaction::fingerprint(&tx, &s.id)?;
            if count < 2 {
                return Ok(PendingClaim::Obsolete {id, revision, reason:"not enough messages to compact"});
            }
            Ok(PendingClaim::Ready(CompactionClaim {
                id,
                incarnation: s.incarnation.clone(),
                fingerprint,
                keep,
                transcript,
            }))
        })?;
        match candidate {
            PendingClaim::Empty => Ok(None),
            PendingClaim::Ready(claim) => Ok(Some(claim)),
            PendingClaim::Obsolete {
                id,
                revision,
                reason,
            } => {
                // The read snapshot has closed. Fence both a replacement request
                // and history that became eligible before acquiring the writer.
                s.write_locked("cancel obsolete compaction", |w| {
                    if w.0.execute("DELETE FROM compaction_request WHERE id=?1 AND (SELECT content_revision FROM manifest)=?2", params![id,revision])? == 1 {
                        w.0.execute("INSERT INTO compaction_cancellations(id,reason) VALUES(?1,?2)",[id.as_str(), reason])?;
                    }
                    Ok(())
                })?;
                Ok(None)
            }
        }
    }

    pub fn finish_compaction(
        &self,
        claim: &CompactionClaim,
        summary: &compaction::GeneratedSummary,
    ) -> Result<String> {
        layout::allow_entry()?;
        let s = &self.session;
        self.check_compaction_session()?;
        ensure!(s.incarnation == claim.incarnation, "Wrong compaction owner");
        ensure!(
            !summary.text.trim().is_empty() && summary.text.chars().count() <= 32_000,
            "Empty or oversized compaction summary"
        );
        s.write_locked("commit session compaction", |w| {
            ensure!(
                w.0.execute("DELETE FROM compaction_request WHERE id=?1", [&claim.id])? == 1,
                "Compaction request changed"
            );
            ensure!(
                compaction::fingerprint(w.0, w.1)?.0 == claim.fingerprint,
                "Session changed during compaction"
            );
            let content = format!(
                "[Conversation summary — earlier transcript retained in session history]\n{}",
                summary.text.trim()
            );
            let message = w.append("user", &content)?;
            if let Some(native) = &summary.native {
                w.native_boundary(&message, native)?;
            } else {
                w.0.execute(
                    "UPDATE messages SET is_compact_boundary=1 WHERE id=?1",
                    [&message],
                )?;
                w.0.execute("UPDATE compaction_state SET local_count=local_count+1", [])?;
            }
            w.0.execute("DELETE FROM compaction_checkpoint", [])?;
            Ok(message)
        })
    }
}
