//! Process-local resolver for the global directory and incarnation-bound session shards.
mod approvals;
mod access;
pub use access::{H5Session,HostAccess};
mod agents;
mod plugins;
pub use plugins::{PluginCatalog,PluginInstall};
pub use agents::AgentCatalog;
mod admission;
mod binding;
mod compaction;
mod directory;
pub mod diagnostics;
mod conversations;
pub use conversations::Rewind;
pub use compaction::{CheckpointKey, CompactionClaim};
pub use directory::{DirectoryEntry, DirectoryFilter, ResumeEntry};
mod delivery;
mod desktop_queue;
mod executor;
mod engine;
mod children;
mod child_reader;
mod worker_directory;
mod peers;
pub(crate) use worker_directory::WorkspaceDirectory;
pub use worker_directory::{WorkerDirectory, WorkerPair};
mod recording;
pub use recording::RecordingPolicies;
mod segment;
mod owned;
mod observation;
mod goal_state;
pub use owned::{SessionAccess,SessionRepository};
pub use segment::{SegmentPlacement, SegmentUpdate};
pub use executor::{ExecutorHistory,HookSessions};
mod global;
mod shared;
mod leases;
pub use leases::ServiceLeases;
mod schedules;
pub use schedules::Schedules;
mod mcp_control;
pub use mcp_control::McpControl;
mod global_format;
mod history;
mod im;
mod im_catalog;
mod learning;
pub use learning::LearningRecords;
pub use im_catalog::ImCatalog;
mod im_permissions;
pub use im_permissions::{ImPermissionChange, ImPermissions};
pub mod magic;
mod documents;
mod team;
pub use team::TeamCatalog;
pub use documents::{DocumentCatalog,DocumentImport};
mod projects;
mod worktrees;
pub use projects::{ProjectEdit,Projects};
mod repair;
mod runtime;
pub mod settings;
mod state;
mod usage;
mod task;
mod task_state;
mod task_roles;
pub use task_roles::TaskMembership;
pub use task_state::TaskState;
pub use delivery::*;
pub use history::*;
pub use state::{GlobalPreferences, GlobalSetting};
pub use usage::{CorpusUsage, DailyUsage};
mod layout;
mod lifecycle;
pub use global::GlobalDb;
pub use lifecycle::{
    Branch, CreateOptions, FirstMessage, ImportResult, Phase, ProjectSelection, SyncHeader,
};

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
    repair: std::sync::OnceLock<std::sync::mpsc::Sender<()>>,
    repair_cursor: Mutex<String>,
    recording: std::sync::RwLock<HashMap<String,crate::native_engine::request_recording::Policy>>,
    upgraded_directory: bool,
    migrating: std::sync::atomic::AtomicBool,
}
type RootSlot = Arc<Mutex<Weak<Root>>>;
static ROOTS: LazyLock<Mutex<HashMap<PathBuf, RootSlot>>> = LazyLock::new(Default::default);

#[derive(Clone)]
pub struct Stores {
    root: Arc<Root>,
    fault: Option<Arc<Fault>>,
}
impl Stores {
    pub fn open(path: &Path) -> Result<Self> {
        layout::allow_entry()?;
        std::fs::create_dir_all(path)?;
        Self::open_existing(path, false)
    }

    #[doc(hidden)]
    pub fn open_for_test(path: &Path) -> Result<Self> {
        Self::open_existing(path, true)
    }

    fn open_existing(path: &Path, reconcile_existing: bool) -> Result<Self> {
        layout::allow_entry()?;
        ensure!(
            path.is_dir(),
            "Session store requires an existing data root"
        );
        let path = path.canonicalize()?;
        let slot = {
            let mut roots = ROOTS.lock().unwrap();
            roots.retain(|_, slot| {
                Arc::strong_count(slot) > 1 || slot.lock().unwrap().strong_count() > 0
            });
            roots.entry(path.clone()).or_default().clone()
        };
        // Opening or migrating one data root must never block another root.
        let mut cached = slot.lock().unwrap();
        if let Some(root) = cached.upgrade() {
            let stores = Self { root, fault: None };
            drop(cached);
            if reconcile_existing {
                drop(stores.namespace()?);
                stores.reconcile()?;
            }
            return Ok(stores);
        }
        let (uuid, upgraded_directory) = layout::open_root(&path)?;
        let mut global = GlobalDb::new(&path, uuid)?;
        let root = Arc::new_cyclic(|weak| {
            global.root = weak.clone();
            Root {
                global,
                upgraded_directory,
                repair: Default::default(),
                repair_cursor: Default::default(),
                recording: Default::default(),
                migrating: Default::default(),
                path,
            }
        });
        let stores = Self { root, fault: None };
        drop(stores.namespace()?);
        stores.reconcile()?;
        stores.start_projection_repair();
        *cached = Arc::downgrade(&stores.root);
        Ok(stores)
    }

    pub fn data_dir(&self) -> &Path {
        &self.root.path
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
        let exclusive = self.group_access(vec![(key, true, false)])?;
        Ok(Some(exclusive))
    }

    pub fn open_session(&self, id: &str) -> Result<SessionDb> {
        let key = storage_key(id)?;
        let mut locks = self.resolved_key(&key)?.context("Session unavailable")?;
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
        let path = self.sessions().join(&handle.key).join("session.db");
        let old = {
            let _permit = Permits::take(1);
            layout::needs_upgrade(&path, SESSION)?
        };
        if old {
            drop(locks);
            locks = self.key_lock(&handle.key, true, false)?;
            handle.check_directory()?;
            handle.upgrade_locked()?;
        }
        let _locks = locks;
        drop(admission::open(&path)?);
        handle.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |_| Ok(()))?;
        handle.try_project_current();
        Ok(handle)
    }
}

#[derive(PartialEq)]
struct VerifiedShard {
    file: (u64, u64),
    schema: i64,
    incarnation: String,
    external_edit: Option<[u8; 16]>,
}
// Verification outlives the last resolver handle, without retaining a root,
// pool or open file. Roots with different stores never share verification.
type VerificationSlot = Arc<Mutex<Option<VerifiedShard>>>;
static VERIFIED: LazyLock<Mutex<HashMap<(PathBuf, String), VerificationSlot>>> =
    LazyLock::new(Default::default);
impl Stores {
    fn verification_slot(&self, key: &str) -> VerificationSlot {
        VERIFIED
            .lock()
            .unwrap()
            .entry((self.root.path.clone(), key.into()))
            .or_default()
            .clone()
    }
    fn forget_verification(&self, key: &str) {
        VERIFIED
            .lock()
            .unwrap()
            .remove(&(self.root.path.clone(), key.into()));
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
    // Requires exclusive lifecycle access to the current incarnation.
    fn upgrade_locked(&self) -> Result<()> {
        let path = self.stores.sessions().join(&self.key).join("session.db");
        let _permit = Permits::take(1);
        if layout::needs_upgrade(&path, SESSION)? {
            let c = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            verify_manifest(
                &c,
                self.stores.global().store_uuid(),
                &self.id,
                &self.incarnation,
            )?;
            drop(c);
            layout::upgrade(&path, SESSION)?;
        }
        Ok(())
    }
    fn connection_under_lock<T>(
        &self,
        mode: OpenFlags,
        body: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        self.connection_with_permit(Permits::take(1), mode, body)
    }
    fn connection_with_permit<T>(
        &self,
        _permit: Permits,
        mode: OpenFlags,
        body: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let path = self.stores.sessions().join(&self.key).join("session.db");
        let file = layout::file_identity(&path)?;
        let mut conn = layout::open(&path, mode)?;
        let snap = conn.transaction()?;
        let (schema, external_edit) = layout::session_header(&snap)?;
        verify_manifest(
            &snap,
            self.stores.global().store_uuid(),
            &self.id,
            &self.incarnation,
        )?;
        ensure!(
            file == layout::file_identity(&path)?,
            "Session file changed during checkout"
        );
        let identity = VerifiedShard {
            file,
            schema,
            incarnation: self.incarnation.clone(),
            external_edit,
        };
        let slot = self.stores.verification_slot(&self.key);
        {
            let mut verified = slot.lock().unwrap();
            if verified.as_ref() != Some(&identity) {
                *verified = None;
                layout::verify(&snap, SESSION)?;
                layout::integrity(&snap)?;
                *verified = Some(identity);
            }
        }
        snap.commit()?;
        body(&mut conn)
    }
    /// Use the supplied transaction for the duration of the callback. Nested
    /// store calls that need lifecycle access return an error on this thread.
    pub fn read<T>(&self, body: impl FnOnce(&SessionRead<'_>) -> Result<T>) -> Result<T> {
        let _locks = self.stores.key_lock(&self.key, false, false)?;
        self.binding_control()?;
        self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
            let tx = c.transaction()?;
            layout::callback(|| body(&SessionRead(&tx)))
        })
    }
    /// The closure follows the same re-entry rules as `read` and may run again
    /// after SQLite contention rolls back its transaction.
    pub fn write<T>(&self, body: impl FnMut(&SessionWrite<'_>) -> Result<T>) -> Result<T> {
        self.write_named("session write", body)
    }
    fn write_named<T>(
        &self,
        label: &str,
        body: impl FnMut(&SessionWrite<'_>) -> Result<T>,
    ) -> Result<T> {
        let _locks = self.stores.key_lock(&self.key, false, false)?;
        self.write_locked(label, body)
    }
    fn write_locked<T>(
        &self,
        label: &str,
        body: impl FnMut(&SessionWrite<'_>) -> Result<T>,
    ) -> Result<T> {
        self.write_locked_with_deadline(label,WRITE_DEADLINE,body)
    }
    fn write_locked_with_deadline<T>(
        &self,
        label: &str,
        deadline: Duration,
        body: impl FnMut(&SessionWrite<'_>) -> Result<T>,
    ) -> Result<T> {
        self.write_locked_with_commit(label,deadline,body,|tx|Ok(tx.commit()?))
    }

    fn write_locked_with_commit<T>(
        &self,
        label: &str,
        deadline: Duration,
        mut body: impl FnMut(&SessionWrite<'_>) -> Result<T>,
        mut commit:impl FnMut(rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<T> {
        let start=Instant::now();
        let _admission=self.provider_fence(false,deadline)?;
        let binding = self.binding_control()?;
        let (result, projection) = self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
            crate::db::write_tx_with_commit(c, label, deadline.saturating_sub(start.elapsed()), |tx| {
                let before:i64=tx.query_row("SELECT total_changes()",[],|r|r.get(0))?;
                let writer=SessionWrite(&tx,&self.id,false.into(),&binding);
                let result=layout::callback(|| body(&writer))?;
                let changed:bool=tx.query_row("SELECT total_changes()>?1",[before],|r|r.get(0))?;
                let mut summary=None;
                if changed {
                    history::advance(&tx)?;
                    if writer.2.get() || !tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_turns WHERE status='running')",[],|r|r.get::<_,bool>(0))? {
                        match history::projection(&tx) {
                            Ok(projection) => summary = Some(projection),
                            Err(error) => tracing::warn!(%error, session=%self.id, "Directory summary could not be computed"),
                        }
                    }
                }
                commit(tx)?;
                Ok((result,summary))
            })
        })
        .map_err(|e| crate::db::write_boundary_context(label, &self.id, start, e))?;
        drop(_admission);
        if let Some(p) = projection {
            if let Err(error) = self
                .stores
                .global()
                .project(&self.id, &self.incarnation, &p)
            {
                tracing::debug!(%error,session=%self.id,"Directory projection pending");
            }
        }
        Ok(result)
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
                 ORDER BY m.sort_order,m.rowid LIMIT 1",
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
pub struct SessionWrite<'a>(&'a Connection, &'a str, std::cell::Cell<bool>, &'a binding::BindingControl);
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
                        (if publication { 2 } else { 1 }, "wal".into(), 1, 0)
                    );
                    Ok(())
                })?;
        }
        // I34 sets busy_timeout to zero after acquiring the writer. The next
        // checkout gets the normal acquisition budget back.
        s.global().read(|c| {
            assert_eq!(pragmas(c)?.3, 5000);
            Ok(())
        })?;
        let _permit = Permits::take(1);
        layout::initialize(&tmp.path().join("stage.db"), SESSION, |tx| {
            assert_eq!(pragmas(tx)?, (2, "wal".into(), 1, 5000));
            Ok(())
        })?;
        drop(_permit);
        s.create("test", CreateOptions::default())?.write(|tx| {
            assert_eq!(pragmas(tx.0)?, (1, "wal".into(), 1, 0));
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
