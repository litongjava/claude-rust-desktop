//! The latest suggestion lives outside the durable message stream.
use super::{settings::Setting, SessionDb, Stores};
use anyhow::Result;
const SUGGESTION: Setting<SavedSuggestion> = Setting::new("prompt.suggestion");

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

    pub fn current(db: &Stores, session: &str) -> Result<Option<Self>> {
        db.open_session(session)?.read(|r| Self::read(r.0, session))
    }
}

/// Snapshot the binding before dispatching the main request. The execution
/// turn ID changes on admission, even before the next user message is saved.
pub struct SuggestionTurn {
    db: SessionDb,
    turn_id: String,
    generation: (u64, i64),
}

fn suggestion_binding(
    s: &SessionDb,
) -> Result<(crate::native_engine::session_binding::store::Observed, i64)> {
    s.stores.global().read(|conn| {
        let tx=conn.unchecked_transaction()?;
        let generation=tx.query_row("SELECT coalesce((SELECT generation FROM conversation_binding_generations WHERE conversation_id=c.id),0) FROM conversations c WHERE c.id=?1 AND c.incarnation=?2",[s.id(),s.incarnation()],|r|r.get(0))?;
        Ok((crate::native_engine::session_binding::store::read(&tx,s.id())?,generation))
    })
}

impl SuggestionTurn {
    pub fn begin(
        db: std::sync::Arc<Stores>,
        session: &str,
        turn_id: &str,
        model: &str,
        provider: &str,
    ) -> Result<Option<Self>> {
        let db = db.open_session(session)?;
        let (observed, revision) = suggestion_binding(&db)?;
        Ok(
            (observed.provider == provider
                && observed.model == model
                && observed.pending.is_none())
            .then(|| Self {
                db,
                turn_id: turn_id.into(),
                generation: (observed.generation, revision),
            }),
        )
    }

    pub fn complete(self, assistant: &str) -> Result<Option<SuggestionGuard>> {
        let Some(position) = self.db.read(|r| HistoryPosition::read(r.0, self.db.id()))? else {
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
                 AND (SELECT count(*) FROM messages WHERE conversation_id=?1)=?3
                 AND (SELECT CASE WHEN role='assistant' THEN id END FROM messages WHERE conversation_id=?1 ORDER BY sort_order DESC,rowid DESC LIMIT 1) IS ?4",
            rusqlite::params![self.turn.db.id(), self.turn.turn_id, self.position.history_len, self.position.assistant_id],
            |row| row.get(0))
    }

    fn with_binding(&self, body: impl FnOnce(&SessionDb) -> Result<bool>) -> Result<bool> {
        let session = &self.turn.db;
        let _key = session.stores.key_lock(&session.key, false, false)?;
        session.check_directory()?;
        let _fence = session.provider_fence(false, crate::db::STARTUP_WRITE_DEADLINE)?;
        let (observed, revision) = suggestion_binding(session)?;
        if (observed.generation, revision) != self.turn.generation || observed.pending.is_some() {
            return Ok(false);
        }
        body(session)
    }

    pub fn is_current(&self) -> Result<bool> {
        self.with_binding(|s| {
            s.connection_under_lock(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
                Ok(self.matches(c)?)
            })
        })
    }

    pub fn save(&self, text: String) -> Result<Option<String>> {
        let saved = SavedSuggestion {
            text,
            created_at: chrono::Utc::now(),
            position: self.position.clone(),
        };
        let published = self.with_binding(|s| {
            s.write_locked("publish prompt suggestion", |w| {
                if !self.matches(w.0)? {
                    return Ok(false);
                }
                w.set_setting(SUGGESTION, &saved)?;
                Ok(true)
            })
        })?;
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
    pub fn load(db: &Stores, session: &str) -> Result<Option<Self>> {
        Self::load_from(&db.open_session(session)?)
    }

    pub fn load_from(s: &SessionDb) -> Result<Option<Self>> {
        s.read(|r| {
            let position = HistoryPosition::read(r.0, s.id())?;
            Ok(r.setting(SUGGESTION)?
                .filter(|saved| Some(&saved.position) == position.as_ref()))
        })
    }

    pub fn save(&self, db: &Stores, session: &str) -> Result<bool> {
        db.open_session(session)?.write(|w| {
            if HistoryPosition::read(w.0, session)?.as_ref() != Some(&self.position) {
                return Ok(false);
            }
            w.set_setting(SUGGESTION, self)?;
            Ok(true)
        })
    }

    pub fn accept(&self, db: &Stores, session: &str) -> Result<bool> {
        self.accept_from(&db.open_session(session)?)
    }

    pub fn accept_from(&self, s: &SessionDb) -> Result<bool> {
        s.write(|w| {
            if w.setting(SUGGESTION)?.as_ref() != Some(self)
                || HistoryPosition::read(w.0, s.id())?.as_ref() != Some(&self.position)
            {
                return Ok(false);
            }
            w.clear_setting(SUGGESTION)?;
            Ok(true)
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimePaths, SessionStore};
    use anyhow::Result;

    #[test]
    fn turn_admission_and_binding_round_trip_invalidate_without_new_history() -> Result<()> {
        use crate::db::turn_repo::{TurnStatus, TurnTimer};
        let root = tempfile::tempdir()?;
        let paths = RuntimePaths::new(root.path().join("data"))?;
        let store = SessionStore::new(paths.open_database()?);
        let other = paths.open_database()?;
        let session = store.create(root.path(), "old", "provider")?;
        let mut turn = store.db.start_turn_timer(&session)?;
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
            other
                .open_session(&session)?
                .edit_conversation(&serde_json::json!({"model":model}))?;
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
        let _next = other.start_turn_timer(&session)?;
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
        let old = store.db.open_session(&session)?;
        old.clone().delete()?;
        assert!(original.accept_from(&old).is_err());
        Ok(())
    }
}
