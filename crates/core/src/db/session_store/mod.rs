//! Session database layout. The sole constructor is an internal integration-test
//! factory until all production hosts switch together.
mod global;
mod layout;
mod lifecycle;
pub use global::GlobalDb;
pub use lifecycle::{CreateOptions, FirstMessage, Phase, ProjectSelection};

use anyhow::{ensure, Context, Result};
use layout::{lock_keys, storage_key, Permits, SESSION};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

const WRITE_DEADLINE: Duration = Duration::from_secs(30);

type Fault = dyn Fn(Phase) -> Result<()> + Send + Sync;
struct Root {
    path: PathBuf,
    global: GlobalDb,
}
static ROOTS: LazyLock<Mutex<HashMap<PathBuf, Weak<Root>>>> = LazyLock::new(Default::default);

#[derive(Clone)]
pub struct Stores {
    root: Arc<Root>,
    fault: Option<Arc<Fault>>,
}
impl Stores {
    #[doc(hidden)]
    pub fn open_for_test(path: &Path) -> Result<Self> {
        layout::allow_entry()?;
        // The caller owns the durability of this data root and its ancestry.
        ensure!(
            path.is_dir(),
            "Session store requires an existing data root"
        );
        let path = path.canonicalize()?;
        let root = {
            let mut roots = ROOTS.lock().unwrap();
            roots.retain(|_, root| root.strong_count() > 0);
            if let Some(root) = roots.get(&path).and_then(Weak::upgrade) {
                root
            } else {
                let uuid = layout::open_root(&path)?;
                let mut global = GlobalDb::new(&path, uuid)?;
                let root = Arc::new_cyclic(|weak| {
                    global.root = weak.clone();
                    Root {
                        global,
                        path: path.clone(),
                    }
                });
                roots.insert(path, Arc::downgrade(&root));
                root
            }
        };
        let stores = Self { root, fault: None };
        drop(stores.namespace()?);
        stores.reconcile()?;
        Ok(stores)
    }
    #[doc(hidden)]
    pub fn with_fault_hook(
        &self,
        fault: impl Fn(Phase) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            root: self.root.clone(),
            fault: Some(Arc::new(fault)),
        }
    }
    fn step(&self, phase: Phase) -> Result<()> {
        if let Some(hook) = &self.fault {
            hook(phase)?;
        }
        Ok(())
    }
    pub fn global(&self) -> &GlobalDb {
        &self.root.global
    }
    fn sessions(&self) -> PathBuf {
        self.root.path.join("sessions")
    }
    fn namespace(&self) -> Result<std::fs::File> {
        let dir = self.root.path.join("session-locks");
        let guard = layout::lock_existing(&dir.join(".directory"), false)?;
        ensure!(
            std::fs::read_to_string(dir.join(".initialized"))? == self.global().uuid,
            "Lifecycle namespace identity changed"
        );
        Ok(guard)
    }

    fn has_directory_state(&self, key: &str) -> Result<bool> {
        self.global().read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE storage_key=?1
            UNION ALL SELECT 1 FROM lifecycle WHERE storage_key=?1)",
                [key],
                |r| r.get(0),
            )?)
        })
    }

    fn key_lock(&self, key: &str, exclusive: bool, allocate: bool) -> Result<Vec<std::fs::File>> {
        self.try_key_lock(key, exclusive, allocate)?
            .context("Session unavailable")
    }

    fn try_key_lock(
        &self,
        key: &str,
        exclusive: bool,
        allocate: bool,
    ) -> Result<Option<Vec<std::fs::File>>> {
        let dir = self.root.path.join("session-locks");
        let path = dir.join(key);
        let namespace = self.namespace()?;
        if !path.try_exists()? {
            let registered = self.has_directory_state(key)?;
            // Another creator may have installed the permanent key while we read G.
            if !path.try_exists()? {
                ensure!(!registered, "Missing lock for registered session");
                if !allocate {
                    // Linearize an unknown-key lookup here. The caller must not
                    // subsequently inspect a newly published G row without a key lock.
                    return Ok(None);
                }
                match std::fs::File::create_new(&path) {
                    Ok(file) => file.sync_all()?,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        // Repeat the barrier on retry if allocation succeeded but fsync failed.
        if allocate {
            layout::sync_dir(&dir)?;
        }
        let mut guards = lock_keys(&dir, vec![key], exclusive)?;
        guards.push(namespace);
        Ok(Some(guards))
    }

    fn resolved_key(&self, key: &str) -> Result<Option<Vec<std::fs::File>>> {
        let Some(shared) = self.try_key_lock(key, false, false)? else {
            return Ok(None);
        };
        if self.pending(key)?.is_none() {
            return Ok(Some(shared));
        }
        drop(shared);
        let exclusive = self.key_lock(key, true, false)?;
        self.recover_key(key)?;
        Ok(Some(exclusive))
    }

    pub fn open_session(&self, id: &str) -> Result<SessionDb> {
        let key = storage_key(id)?;
        let _locks = self.resolved_key(&key)?.context("Session unavailable")?;
        let incarnation = self
            .global()
            .incarnation(id, &key)?
            .context("Session unavailable")?;
        let handle = SessionDb {
            stores: self.clone(),
            id: id.into(),
            key,
            incarnation,
        };
        handle.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| layout::integrity(c))?;
        Ok(handle)
    }
}

#[derive(Clone)]
pub struct SessionDb {
    stores: Stores,
    id: String,
    key: String,
    incarnation: String,
}

impl SessionDb {
    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }
    fn check_directory(&self) -> Result<()> {
        let current = self.stores.global().incarnation(&self.id, &self.key)?;
        ensure!(
            current.as_deref() == Some(&self.incarnation),
            "Session incarnation is no longer visible"
        );
        Ok(())
    }
    fn connection_under_lock<T>(
        &self,
        mode: OpenFlags,
        body: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let _permit = Permits::take(1);
        let mut conn = layout::open(
            &self.stores.sessions().join(&self.key).join("session.db"),
            mode,
        )?;
        layout::verify(&conn, SESSION)?;
        verify_manifest(
            &conn,
            self.stores.global().store_uuid(),
            &self.id,
            &self.incarnation,
        )?;
        body(&mut conn)
    }
    /// Use the supplied transaction for the duration of the callback. Nested
    /// store calls that need lifecycle access return an error on this thread.
    pub fn read<T>(&self, body: impl FnOnce(&SessionRead<'_>) -> Result<T>) -> Result<T> {
        let _locks = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
            let tx = c.transaction()?;
            layout::callback(|| body(&SessionRead(&tx)))
        })
    }
    /// The closure follows the same re-entry rules as `read` and may run again
    /// after SQLite contention rolls back its transaction.
    pub fn write<T>(&self, mut body: impl FnMut(&SessionWrite<'_>) -> Result<T>) -> Result<T> {
        let start = Instant::now();
        let _locks = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
            crate::db::write_tx(c, "session write", WRITE_DEADLINE, |tx| {
                layout::callback(|| body(&SessionWrite(tx)))
            })
        })
        .map_err(|e| crate::db::write_boundary_context("session write", &self.id, start, e))
    }
    pub fn set_archived(&self, archived: bool) -> Result<()> {
        let _locks = self.stores.key_lock(&self.key, true, false)?;
        self.check_directory()?;
        self.stores
            .global()
            .write("archive session", &self.id, false, |tx| {
                tx.execute(
                    "UPDATE conversations SET archived=?1,metadata_revision=metadata_revision+1
                WHERE id=?2 AND incarnation=?3 AND archived<>?1",
                    rusqlite::params![archived, self.id, self.incarnation],
                )?;
                Ok(())
            })
    }
}

fn verify_manifest(c: &Connection, store: &str, id: &str, incarnation: &str) -> Result<()> {
    let matches: bool = c.query_row(
        "SELECT store_uuid=?1 AND id=?2 AND incarnation=?3 FROM manifest WHERE singleton=1",
        [store, id, incarnation],
        |r| r.get(0),
    )?;
    if !matches {
        return Err(layout::InvalidFormat("Session identity mismatch".into()).into());
    }
    Ok(())
}

pub struct SessionRead<'a>(&'a Connection);
impl SessionRead<'_> {
    pub fn initial_message(&self) -> Result<Option<FirstMessage>> {
        let pair = self
            .0
            .query_row(
                "SELECT m.id,m.conversation_id,m.role,m.content,m.thinking,m.created_at,
                        m.is_compact_boundary,m.sort_order,t.message_id,t.content_json
                 FROM messages m LEFT JOIN message_user_content t ON t.message_id=m.id
                 WHERE m.sort_order=0",
                [],
                |row| {
                    let message = crate::db::message_repo::row_to_message(row)?;
                    let json = match row.get::<_, Option<String>>(8)? {
                        Some(_) => Some(row.get::<_, String>(9)?),
                        None => None,
                    };
                    Ok((message.content, json))
                },
            )
            .optional()?;
        pair.map(|(text, json)| {
            Ok(FirstMessage {
                text,
                typed: json.map(|value| serde_json::from_str(&value)).transpose()?,
            })
        })
        .transpose()
    }
}
pub struct SessionWrite<'a>(&'a Connection);
impl SessionWrite<'_> {
    #[doc(hidden)]
    pub fn synchronous_for_test(&self) -> Result<i32> {
        Ok(self
            .0
            .pragma_query_value(None, "synchronous", |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragmas(conn: &Connection) -> Result<(i32, String, i32, i32)> {
        Ok(conn.query_row(
            "SELECT synchronous,journal_mode,foreign_keys,timeout FROM
            pragma_synchronous,pragma_journal_mode,pragma_foreign_keys,pragma_busy_timeout",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?)
    }

    #[test]
    fn durability_and_busy_policy_follow_the_transaction_role() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let s = Stores::open_for_test(tmp.path())?;
        for publication in [true, false, true] {
            s.global()
                .write("inspect pragma", "test", publication, |tx| {
                    assert_eq!(
                        pragmas(tx)?,
                        (if publication { 2 } else { 1 }, "wal".into(), 1, 5000)
                    );
                    Ok(())
                })?;
        }
        let _permit = Permits::take(1);
        layout::initialize(&tmp.path().join("stage.db"), SESSION, |tx| {
            assert_eq!(pragmas(tx)?, (2, "wal".into(), 1, 5000));
            Ok(())
        })?;
        drop(_permit);
        s.create("test", CreateOptions::default())?.write(|tx| {
            assert_eq!(pragmas(tx.0)?, (1, "wal".into(), 1, 5000));
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn multi_key_locks_are_unique_and_in_storage_key_order() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("session-locks");
        std::fs::create_dir(&path)?;
        for name in ["a", "b"] {
            std::fs::File::create_new(path.join(name))?;
        }
        let locks = lock_keys(&path, vec!["b", "a", "b"], true)?;
        assert_eq!(locks.len(), 2);
        for (file, name) in locks.iter().zip(["a", "b"]) {
            assert_eq!(
                file.metadata()?.ino(),
                std::fs::metadata(path.join(name))?.ino()
            );
        }
        Ok(())
    }
}
