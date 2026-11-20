//! The latest suggestion lives outside the durable message stream.
use anyhow::{Context, Result};

/// The saved suggestion is meaningful only at this exact end of history.
#[derive(Debug, PartialEq, Eq, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryPosition {
    pub assistant_id: String,
    pub history_len: usize,
}

impl HistoryPosition {
    fn read(conn: &rusqlite::Connection, session: &str) -> Result<Option<Self>> {
        use rusqlite::OptionalExtension;
        Ok(conn
            .query_row(
                "SELECT id, role, (SELECT count(*) FROM messages WHERE conversation_id=?1)
             FROM messages WHERE conversation_id=?1 ORDER BY sort_order DESC, rowid DESC LIMIT 1",
                [session],
                |row| {
                    Ok((
                        row.get::<_, String>(1)?,
                        Self {
                            assistant_id: row.get(0)?,
                            history_len: row.get(2)?,
                        },
                    ))
                },
            )
            .optional()?
            .and_then(|(role, position)| (role == "assistant").then_some(position)))
    }

    pub fn current(db: &crate::db::DbManager, session: &str) -> Result<Option<Self>> {
        db.with_conn(|conn| Self::read(conn, session))?
    }
}

/// Snapshot the binding before dispatching the main request. The execution
/// turn ID changes on admission, even before the next user message is saved.
pub struct SuggestionTurn {
    db: std::sync::Arc<crate::db::DbManager>,
    session: String,
    turn_id: String,
    generation: i64,
}

impl SuggestionTurn {
    pub fn begin(
        db: std::sync::Arc<crate::db::DbManager>,
        session: &str,
        turn_id: &str,
        model: &str,
        provider: &str,
    ) -> Result<Option<Self>> {
        use rusqlite::OptionalExtension;
        let generation = db.with_conn(|conn| {
            conn.query_row(
                "SELECT coalesce(g.generation, 0) FROM conversations c
             LEFT JOIN conversation_binding_generations g ON g.conversation_id=c.id
             WHERE c.id=?1 AND c.model=?2 AND c.provider=?3",
                [session, model, provider],
                |row| row.get(0),
            )
            .optional()
        })??;
        Ok(generation.map(|generation| Self {
            db,
            session: session.into(),
            turn_id: turn_id.into(),
            generation,
        }))
    }

    pub fn complete(self, assistant: &str) -> Result<Option<SuggestionGuard>> {
        let Some(position) = HistoryPosition::current(&self.db, &self.session)? else {
            return Ok(None);
        };
        Ok(
            (position.assistant_id == assistant).then_some(SuggestionGuard {
                turn: self,
                position,
            }),
        )
    }
}

pub struct SuggestionGuard {
    turn: SuggestionTurn,
    position: HistoryPosition,
}

impl std::fmt::Debug for SuggestionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SuggestionGuard")
            .field("turn_id", &self.turn.turn_id)
            .field("position", &self.position)
            .finish()
    }
}

impl SuggestionGuard {
    fn matches(&self, conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT (SELECT CASE WHEN status='completed' THEN id END FROM execution_turns WHERE conversation_id=?1 ORDER BY rowid DESC LIMIT 1) IS ?2
                 AND coalesce((SELECT generation FROM conversation_binding_generations WHERE conversation_id=?1),0)=?3
                 AND (SELECT count(*) FROM messages WHERE conversation_id=?1)=?4
                 AND (SELECT CASE WHEN role='assistant' THEN id END FROM messages WHERE conversation_id=?1 ORDER BY sort_order DESC,rowid DESC LIMIT 1) IS ?5",
            rusqlite::params![self.turn.session, self.turn.turn_id, self.turn.generation, self.position.history_len, self.position.assistant_id],
            |row| row.get(0))
    }

    pub fn is_current(&self) -> Result<bool> {
        Ok(self.turn.db.with_conn(|conn| self.matches(conn))??)
    }

    pub fn save(&self, text: String) -> Result<Option<String>> {
        let saved = SavedSuggestion {
            text,
            created_at: chrono::Utc::now(),
            position: self.position.clone(),
        };
        let published =
            saved.save_if(
                &self.turn.db,
                &self.turn.session,
                |tx| Ok(self.matches(tx)?),
            )?;
        Ok(published.then_some(saved.text))
    }
}

#[derive(Debug, PartialEq, Eq, Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedSuggestion {
    pub text: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(flatten)]
    pub position: HistoryPosition,
}

impl SavedSuggestion {
    fn path(db: &crate::db::DbManager, session: &str) -> Result<std::path::PathBuf> {
        use crate::session_files::*;
        anyhow::ensure!(is_safe_session(session), "Invalid session ID");
        Ok(environment_file(db.data_dir(), session, SUGGESTION_SUFFIX))
    }

    fn read(path: &std::path::Path) -> Result<Option<Self>> {
        match std::fs::File::open(path) {
            Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn load(db: &crate::db::DbManager, session: &str) -> Result<Option<Self>> {
        let Some(saved) = Self::read(&Self::path(db, session)?)? else {
            return Ok(None);
        };
        Ok(
            (HistoryPosition::current(db, session)?.as_ref() == Some(&saved.position))
                .then_some(saved),
        )
    }

    pub fn save(&self, db: &crate::db::DbManager, session: &str) -> Result<bool> {
        self.save_if(db, session, |tx| {
            Ok(HistoryPosition::read(tx, session)?.as_ref() == Some(&self.position))
        })
    }

    fn save_if(
        &self,
        db: &crate::db::DbManager,
        session: &str,
        current: impl FnOnce(&rusqlite::Connection) -> Result<bool>,
    ) -> Result<bool> {
        let path = Self::path(db, session)?;
        db.with_conn(|conn| -> Result<bool> {
            // Serialize the comparison and publication with other turns and deletion.
            let tx = crate::db::write_transaction(conn)?;
            if !current(&tx)? {
                return Ok(false);
            }
            let dir = path.parent().context("Missing session directory")?;
            std::fs::create_dir_all(dir)?;
            let mut temp = tempfile::NamedTempFile::new_in(dir)?;
            serde_json::to_writer(temp.as_file_mut(), self)?;
            temp.persist(path)?;
            tx.commit()?;
            Ok(true)
        })?
    }

    /// A delayed keypress must never consume a newer suggestion.
    pub fn accept(&self, db: &crate::db::DbManager, session: &str) -> Result<bool> {
        let path = Self::path(db, session)?;
        db.with_conn(|conn| -> Result<bool> {
            let tx = crate::db::write_transaction(conn)?;
            if Self::read(&path)?.as_ref() != Some(self)
                || HistoryPosition::read(&tx, session)?.as_ref() != Some(&self.position)
            {
                return Ok(false);
            }
            std::fs::remove_file(path)?;
            tx.commit()?;
            Ok(true)
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimePaths, SessionStore};

    #[test]
    fn turn_admission_and_binding_round_trip_invalidate_without_new_history() -> Result<()> {
        use crate::db::turn_repo::{TurnStatus, TurnTimer};
        let root = tempfile::tempdir()?;
        let paths = RuntimePaths::new(root.path().join("data"))?;
        let store = SessionStore::new(paths.open_database()?);
        let other = paths.open_database()?;
        let session = store.create(root.path(), "old", "provider")?;
        let mut turn = TurnTimer::start(store.db.clone(), &session)?;
        let begin = || {
            SuggestionTurn::begin(
                store.db.clone(),
                &session,
                &turn.record().turn_id,
                "old",
                "provider",
            )
        };
        let origin = begin()?.unwrap();
        let assistant = turn.ensure_segment()?;
        turn.finish(TurnStatus::Completed)?;
        let guard = origin.complete(&assistant)?.unwrap();
        let change = |model: &str| -> Result<()> {
            other.with_conn(|c| {
                c.execute(
                    "UPDATE conversations SET model=?1 WHERE id=?2",
                    [model, &session],
                )
            })??;
            Ok(())
        };
        change("old")?;
        assert!(guard.is_current()?);
        change("other")?;
        change("old")?;
        assert!(!guard.is_current()?);
        assert!(guard.save("old suggestion".into())?.is_none());
        let guard = SuggestionTurn::begin(
            store.db.clone(),
            &session,
            &turn.record().turn_id,
            "old",
            "provider",
        )?
        .unwrap()
        .complete(&assistant)?
        .unwrap();
        assert!(guard.is_current()?);
        let _next = TurnTimer::start(other, &session)?;
        assert_eq!(
            HistoryPosition::current(&store.db, &session)?.unwrap(),
            guard.position
        );
        assert!(!guard.is_current()?);
        assert!(guard.save("old suggestion".into())?.is_none());
        Ok(())
    }

    #[test]
    fn replacement_acceptance_stale_history_and_deletion() -> Result<()> {
        let root = tempfile::tempdir()?;
        let paths = RuntimePaths::new(root.path().join("data"))?;
        let store = SessionStore::new(paths.open_database()?);
        let session = store.create(root.path(), "model", "provider")?;
        store.append(&session, "user", "check the builds")?;
        let id = store.append(&session, "assistant", "checked")?;
        let history = store.history(&session)?;
        let transcript = || {
            std::fs::read(
                crate::services::lifecycle_events::transcript(&paths.data_dir, &session).unwrap(),
            )
            .unwrap()
        };
        let original_transcript = transcript();
        let original = SavedSuggestion {
            text: "run the tests".into(),
            created_at: chrono::Utc::now(),
            position: HistoryPosition {
                assistant_id: id,
                history_len: 2,
            },
        };
        assert!(original.save(&store.db, &session)?);
        let replacement = SavedSuggestion {
            text: "check the logs".into(),
            ..original.clone()
        };
        assert!(replacement.save(&store.db, &session)?);
        assert!(!original.accept(&store.db, &session)?);
        assert_eq!(
            SavedSuggestion::load(&store.db, &session)?,
            Some(replacement.clone())
        );
        assert!(replacement.accept(&store.db, &session)?);
        assert!(SavedSuggestion::load(&store.db, &session)?.is_none());
        assert!(original.save(&store.db, &session)?);
        assert_eq!(history, store.history(&session)?);
        assert_eq!(original_transcript, transcript());
        store.append(&session, "user", "something else")?;
        assert!(SavedSuggestion::load(&store.db, &session)?.is_none());
        assert!(!replacement.save(&store.db, &session)?);
        let file = SavedSuggestion::path(&store.db, &session)?;
        assert!(file.exists());
        store.db.with_conn(|conn| {
            crate::db::conversation_repo::delete_conversation(conn, &session)
        })??;
        crate::prompt::forget_session_files(&paths.data_dir, &session);
        assert!(!file.exists());
        assert!(!original.save(&store.db, &session)?);
        assert!(!file.exists());
        Ok(())
    }
}
