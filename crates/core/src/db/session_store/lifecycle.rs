use super::layout::{self, canonical_uuid, durable_move, storage_key, sync_dir, Permits, SESSION};
use super::{verify_manifest, SessionDb, Stores};
use crate::db::project_repo;
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    BeforeCreateIntentCommit,
    CreateIntent,
    StageDirectory,
    BeforeSeal,
    Sealed,
    Checkpointed,
    Renamed,
    Installed,
    BeforePublishCommit,
    Published,
    BeforeDeleteIntentCommit,
    DeleteIntent,
    TrashRenamed,
    Trashed,
    BeforeTrashRemoval,
    Removed,
    Retired,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FirstMessage {
    pub text: String,
    pub typed: Option<serde_json::Value>,
}
#[derive(Clone, Debug)]
pub enum ProjectSelection {
    Existing(String),
    Workspace(PathBuf),
}
#[derive(Clone, Debug, Default)]
pub struct CreateOptions {
    pub title: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub workspace_mode: Option<String>,
    pub research: bool,
    pub project: Option<ProjectSelection>,
    pub first_message: Option<FirstMessage>,
}

#[derive(Serialize, Deserialize)]
struct Metadata {
    // Prior v1 intents recorded only the manifest count.
    #[serde(default)]
    initial_shape: Option<(bool, bool)>,
    title: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    mode: String,
    research: bool,
    created: String,
    project: Option<Project>,
}
#[derive(Serialize, Deserialize)]
enum Project {
    Selected { id: String, folders: Vec<String> },
    NewWorkspace(String),
}

impl Project {
    fn select(conn: &Connection, selection: &ProjectSelection) -> Result<Self> {
        let id = match selection {
            ProjectSelection::Existing(id) => id.clone(),
            ProjectSelection::Workspace(path) => {
                let folder =
                    project_repo::normalize_folders(&[path.to_string_lossy().into_owned()])?
                        .remove(0);
                match Self::matching(conn, &folder)? {
                    Some(id) => id,
                    None => return Ok(Self::NewWorkspace(folder)),
                }
            }
        };
        let p = project_repo::require_trusted(conn, &id)?;
        ensure!(!p.is_archived, "Project is archived");
        Ok(Self::Selected {
            id,
            folders: p.folders.into_iter().map(|f| f.path).collect(),
        })
    }

    fn matching(c: &Connection, path: &str) -> Result<Option<String>> {
        Ok(c.query_row(
            "SELECT p.id FROM projects p JOIN project_folders f ON f.project_id=p.id
            WHERE p.is_archived=0 AND p.workspace_path=?1 AND f.path=?1 AND f.trusted=1
            AND (SELECT count(*) FROM project_folders WHERE project_id=p.id)=1
            ORDER BY p.updated_at DESC,p.id LIMIT 1",
            [path],
            |r| r.get(0),
        )
        .optional()?)
    }

    fn publish(&self, tx: &Connection, now: &str) -> Result<(String, Option<String>)> {
        match self {
            Self::Selected { id, folders } => {
                let p = project_repo::require_trusted(tx, id)?;
                ensure!(
                    !p.is_archived && p.folders.iter().map(|f| &f.path).eq(folders),
                    "Project selection changed"
                );
                Ok((id.clone(), p.workspace_path))
            }
            Self::NewWorkspace(path) => {
                ensure!(
                    project_repo::normalize_folders(std::slice::from_ref(path))?[0] == *path,
                    "Workspace path changed"
                );
                if let Some(id) = Self::matching(tx, path)? {
                    return Ok((id, Some(path.clone())));
                }
                let conflict: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM projects WHERE workspace_path=?1 AND is_archived=0)",
                    [path], |r| r.get(0))?;
                ensure!(!conflict, "Workspace project selection changed");
                let id = Uuid::new_v4().to_string();
                let name = Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                project_repo::insert_project(
                    tx,
                    &id,
                    &name,
                    None,
                    None,
                    Some(path),
                    false,
                    now,
                    now,
                )?;
                tx.execute("INSERT INTO project_folders VALUES(?1,?2,1,0)", [&id, path])?;
                Ok((id, Some(path.clone())))
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
enum QuarantineSource {
    Stage,
    Live,
    Trash,
}
#[derive(Serialize, Deserialize)]
struct Quarantine {
    source: QuarantineSource,
    reason: String,
}

pub(super) struct Intent {
    operation: String,
    store: String,
    id: String,
    key: String,
    incarnation: String,
    kind: String,
    metadata: Option<String>,
}
impl Intent {
    fn new(stores: &Stores, id: &str, key: &str, incarnation: String, kind: &str) -> Self {
        Self {
            operation: Uuid::new_v4().to_string(),
            store: stores.global().uuid.clone(),
            id: id.into(),
            key: key.into(),
            incarnation,
            kind: kind.into(),
            metadata: None,
        }
    }
    fn insert(&self, tx: &Connection, metadata: Option<&str>) -> Result<()> {
        tx.execute(
            "INSERT INTO lifecycle(operation,store_uuid,id,storage_key,incarnation,kind,metadata)
            VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                self.operation,
                self.store,
                self.id,
                self.key,
                self.incarnation,
                self.kind,
                metadata
            ],
        )?;
        Ok(())
    }
    fn stage(&self, stores: &Stores) -> PathBuf {
        stores.sessions().join(".staging").join(&self.operation)
    }
    fn live(&self, stores: &Stores) -> PathBuf {
        stores.sessions().join(&self.key)
    }
    fn finish(&self, tx: &Connection) -> Result<()> {
        ensure!(tx.execute("DELETE FROM lifecycle WHERE operation=?1 AND store_uuid=?2 AND incarnation=?3 AND quarantine_reason IS NULL",
            [&self.operation,&self.store,&self.incarnation])? == 1, "Lifecycle intent changed");
        Ok(())
    }
    fn validate(&self, stores: &Stores) -> Result<()> {
        ensure!(
            self.store == stores.global().uuid
                && self.key == storage_key(&self.id)?
                && canonical_uuid(&self.incarnation)
                && canonical_uuid(&self.operation),
            "Invalid lifecycle identity"
        );
        Ok(())
    }
}

impl Stores {
    pub fn create(&self, id: &str, options: CreateOptions) -> Result<SessionDb> {
        let key = storage_key(id)?;
        let _locks = self.key_lock(&key, true, true)?;
        self.recover_key(&key)?;
        self.global().read(|conn| {
            let fenced: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM lifecycle WHERE storage_key=?1
                AND kind='delete' AND quarantine_reason IS NOT NULL)",
                [&key],
                |r| r.get(0),
            )?;
            ensure!(
                !fenced,
                "Quarantined delete requires resolution before ID reuse"
            );
            Ok(())
        })?;
        let op = Intent::new(self, id, &key, Uuid::new_v4().to_string(), "create");
        ensure!(
            !op.live(self).try_exists()?,
            "Session directory already exists"
        );
        let mode = options.workspace_mode.unwrap_or_else(|| "chat".into());
        ensure!(
            matches!(mode.as_str(), "chat" | "work" | "study" | "code"),
            "Invalid workspace mode"
        );
        let created = chrono::Utc::now().to_rfc3339();
        let meta = self.global().write("create intent", id, true, |tx| {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 OR storage_key=?2)",
                [id, &key],
                |r| r.get(0),
            )?;
            ensure!(!exists, "Session already exists");
            let meta = Metadata {
                initial_shape: Some((
                    options.first_message.is_some(),
                    options
                        .first_message
                        .as_ref()
                        .is_some_and(|m| m.typed.is_some()),
                )),
                title: options.title.clone(),
                model: options.model.clone(),
                provider: options.provider.clone(),
                mode: mode.clone(),
                research: options.research,
                created: created.clone(),
                project: options
                    .project
                    .as_ref()
                    .map(|p| Project::select(tx, p))
                    .transpose()?,
            };
            op.insert(tx, Some(&serde_json::to_string(&meta)?))?;
            self.step(Phase::BeforeCreateIntentCommit)?;
            Ok(meta)
        })?;
        self.step(Phase::CreateIntent)?;
        let stage = op.stage(self);
        fs::create_dir(&stage)?;
        sync_dir(stage.parent().unwrap())?;
        self.step(Phase::StageDirectory)?;
        {
            let _permit = Permits::take(1);
            let sealed = layout::initialize(&stage.join("session.db"), SESSION, |tx| {
                let n = i64::from(options.first_message.is_some());
                tx.execute(
                    "INSERT INTO manifest VALUES(1,?1,?2,?3,?4,?5,?5,?6,?6,NULL)",
                    params![op.store, id, op.incarnation, op.operation, created, n],
                )?;
                if let Some(first) = &options.first_message {
                    let message = Uuid::new_v4().to_string();
                    tx.execute("INSERT INTO messages(id,conversation_id,role,content,created_at,sort_order)
                        VALUES(?1,?2,'user',?3,?4,0)",params![message,id,first.text,created])?;
                    if let Some(typed) = &first.typed {
                        tx.execute(
                            "INSERT INTO message_user_content VALUES(?1,?2)",
                            params![message, serde_json::to_string(typed)?],
                        )?;
                    }
                }
                self.step(Phase::BeforeSeal)
            })?;
            self.step(Phase::Sealed)?;
            sealed.close().map_err(|(_, e)| e)?;
        }
        self.publish(&op, &meta, &stage)?;
        Ok(SessionDb {
            stores: self.clone(),
            id: id.into(),
            key,
            incarnation: op.incarnation,
        })
    }

    pub(super) fn pending(&self, key: &str) -> Result<Option<Intent>> {
        self.global().read(|c| {
            Ok(c.query_row(
                "SELECT operation,store_uuid,id,storage_key,incarnation,kind,metadata
            FROM lifecycle WHERE storage_key=?1 AND quarantine_reason IS NULL",
                [key],
                |r| {
                    Ok(Intent {
                        operation: r.get(0)?,
                        store: r.get(1)?,
                        id: r.get(2)?,
                        key: r.get(3)?,
                        incarnation: r.get(4)?,
                        kind: r.get(5)?,
                        metadata: r.get(6)?,
                    })
                },
            )
            .optional()?)
        })
    }

    pub(super) fn recover_key(&self, key: &str) -> Result<()> {
        let Some(op) = self.pending(key)? else {
            return Ok(());
        };
        op.validate(self)?;
        let stage = op.stage(self);
        let live = op.live(self);
        let dir = if stage.try_exists()? { &stage } else { &live };
        if let Some(meta) = op.metadata.as_deref() {
            let value: serde_json::Value = serde_json::from_str(meta)?;
            if let Some(disposition) = value.get("quarantine") {
                let disposition: Quarantine = serde_json::from_value(disposition.clone())?;
                return self.finish_quarantine(&op, &disposition);
            }
        }
        let recovered_quarantine = self.sessions().join(".quarantine").join(&op.operation);
        if recovered_quarantine.try_exists()? {
            // A pre-fix interrupted move has no saved source. Cover each
            // possible source directory before acknowledging the visible name.
            sync_dir(recovered_quarantine.parent().unwrap())?;
            sync_dir(&self.sessions())?;
            sync_dir(&self.sessions().join(".staging"))?;
            sync_dir(&self.sessions().join(".trash"))?;
            return self.record_quarantine(&op, "interrupted quarantine");
        }
        if op.kind == "delete" {
            return self.retire(&op);
        }
        ensure!(op.kind == "create", "Unknown intent");
        let visible = self.global().read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE storage_key=?1 OR id=?2)",
                [&op.key, &op.id],
                |r| r.get::<_, bool>(0),
            )?)
        })?;
        if visible {
            if stage.try_exists()? {
                self.quarantine(&op, &stage, "occupied directory row")?;
            } else {
                self.record_quarantine(&op, "occupied directory row")?;
            }
            return Ok(());
        }
        let db = dir.join("session.db");
        let unsealed = if db.try_exists()? {
            match self.unsealed(&db) {
                Ok(value) => value,
                Err(e) if damaged_shard(&e) => {
                    return self.quarantine(&op, dir, &format!("invalid staged database: {e:#}"))
                }
                Err(e) => return Err(e),
            }
        } else {
            true
        };
        if unsealed {
            if dir == &live && live.try_exists()? {
                return self.quarantine(&op, &live, "Missing or unsealed installed database");
            }
            // Only discard our private, unsealed stage. Never remove a final
            // directory on the strength of a create intent.
            if stage.try_exists()? {
                fs::remove_dir_all(&stage)?;
            }
            sync_dir(stage.parent().unwrap())?;
            return self
                .global()
                .write("discard unsealed create", &op.id, true, |tx| op.finish(tx));
        }
        let meta =
            serde_json::from_str(op.metadata.as_deref().context("Missing create metadata")?)?;
        self.publish(&op, &meta, dir)
    }

    fn unsealed(&self, db: &Path) -> Result<bool> {
        let _permit = Permits::take(1);
        let c = layout::open(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // Schema, version and seal are installed in a single transaction.
        // A rolled back first transaction leaves a zero-schema SQLite file.
        Ok(c.query_row("SELECT application_id=0 AND NOT EXISTS(SELECT 1 FROM sqlite_schema) FROM pragma_application_id",[], |r|r.get(0))?)
    }

    fn quarantine(&self, op: &Intent, dir: &Path, reason: &str) -> Result<()> {
        let source = if dir == op.stage(self) {
            QuarantineSource::Stage
        } else if dir == op.live(self) {
            QuarantineSource::Live
        } else {
            QuarantineSource::Trash
        };
        let disposition = Quarantine {
            source,
            reason: reason.into(),
        };
        self.global().write("journal quarantine", &op.id, true, |tx| {
            tx.execute("UPDATE lifecycle SET metadata=json_set(COALESCE(metadata,'{}'),'$.quarantine',json(?1)) WHERE operation=?2",
                params![serde_json::to_string(&disposition)?,op.operation])?;
            Ok(())
        })?;
        self.finish_quarantine(op, &disposition)
    }

    fn finish_quarantine(&self, op: &Intent, disposition: &Quarantine) -> Result<()> {
        let source = match disposition.source {
            QuarantineSource::Stage => op.stage(self),
            QuarantineSource::Live => op.live(self),
            QuarantineSource::Trash => self.sessions().join(".trash").join(&op.incarnation),
        };
        let dest = self.sessions().join(".quarantine").join(&op.operation);
        if !dest.try_exists()? {
            layout::rename(&source, &dest)?;
        }
        sync_dir(dest.parent().unwrap())?;
        sync_dir(source.parent().unwrap())?;
        self.record_quarantine(op, &disposition.reason)
    }

    fn record_quarantine(&self, op: &Intent, reason: &str) -> Result<()> {
        self.global()
            .write("record quarantine", &op.id, true, |tx| {
                tx.execute(
                    "UPDATE lifecycle SET quarantine_reason=?1 WHERE operation=?2",
                    [reason, &op.operation],
                )?;
                Ok(())
            })
    }

    fn publish(&self, op: &Intent, meta: &Metadata, dir: &Path) -> Result<()> {
        let live = op.live(self);
        let db = dir.join("session.db");
        let inspect = || -> Result<_> {
            let _permit = Permits::take(1);
            let c = layout::open(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            layout::verify(&c, SESSION)?;
            layout::integrity(&c)?;
            let matches: bool = c.query_row(
                "SELECT store_uuid=?1 AND id=?2 AND incarnation=?3 AND sealed_operation=?4
                FROM manifest WHERE singleton=1",
                [&op.store, &op.id, &op.incarnation, &op.operation],
                |r| r.get(0),
            )?;
            if !matches {
                return Err(layout::InvalidFormat("Sealed identity mismatch".into()).into());
            }
            let p: (i64,i64,Option<String>,i64) = c.query_row("SELECT message_count,content_revision,
                (SELECT substr(content,1,80) FROM messages WHERE sort_order=0),
                coalesce((SELECT length(CAST(content AS BLOB)) FROM messages WHERE sort_order=0),0) FROM manifest",
                [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            let history_ok: bool = c.query_row("SELECT
                m.message_count IN (0,1) AND m.content_revision=m.message_count
                AND m.message_count=(SELECT count(*) FROM messages)
                AND NOT EXISTS(SELECT 1 FROM messages WHERE conversation_id IS NOT m.id
                    OR role IS NOT 'user' OR sort_order IS NOT 0 OR typeof(content)<>'text'
                    OR created_at IS NOT m.created_at)
                AND NOT EXISTS(SELECT 1 FROM pragma_foreign_key_check)
                AND NOT EXISTS(SELECT 1 FROM message_user_content WHERE NOT json_valid(content_json))
                AND (?1 IS NULL OR m.message_count=?1)
                AND (?2 IS NULL OR (SELECT count(*) FROM message_user_content)=?2)
                FROM manifest m WHERE singleton=1",
                params![meta.initial_shape.map(|v|v.0),meta.initial_shape.map(|v|v.1)],|r|r.get(0))?;
            if !history_ok {
                return Err(layout::InvalidFormat(
                    "Initial history differs from sealed projection".into(),
                )
                .into());
            }
            drop(c);
            layout::checkpoint(&db, SESSION)?;
            Ok(p)
        };
        let projection = match inspect() {
            Ok(p) => p,
            Err(e) if damaged_shard(&e) => {
                self.quarantine(op, dir, &format!("invalid sealed database: {e:#}"))?;
                return Err(e);
            }
            Err(e) => return Err(e),
        };

        self.step(Phase::Checkpointed)?;
        if dir != live {
            if live.try_exists()? {
                self.quarantine(op, dir, "occupied destination")?;
                bail!("Session destination occupied");
            }
            layout::rename(dir, &live)?;
            self.step(Phase::Renamed)?;
        }
        // Also repeat both barriers on replay after the rename.
        sync_dir(&self.sessions())?;
        sync_dir(&self.sessions().join(".staging"))?;
        self.step(Phase::Installed)?;
        let mut project_conflict = false;
        let result = self.global().write("publish session", &op.id, true, |tx| {
            let (project,workspace) = match &meta.project {
                None => (None,None),
                Some(p) => {
                    let (id, path) = p.publish(tx, &meta.created).inspect_err(|e| {
                        project_conflict = e.downcast_ref::<rusqlite::Error>().is_none();
                    })?;
                    (Some(id), path)
                }
            };
            tx.execute("INSERT INTO conversations(id,storage_key,incarnation,title,model,provider,workspace_path,project_id,
                workspace_mode,research_mode,created_at,updated_at,message_count,content_revision,first_prompt,latest_prompt,transcript_bytes)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11,?12,?13,?14,?14,?15)",
                params![op.id,op.key,op.incarnation,meta.title,meta.model,meta.provider,workspace,project,meta.mode,meta.research,
                    meta.created,projection.0,projection.1,projection.2,projection.3])?;
            op.finish(tx)?;
            self.step(Phase::BeforePublishCommit)
        });
        if let Err(e) = result {
            // Validation and constraint conflicts are permanent. IO, busy, and
            // other SQLite errors retain the sealed operation for retry.
            let sql = e.downcast_ref::<rusqlite::Error>();
            if project_conflict
                || sql.and_then(|e| e.sqlite_error_code())
                    == Some(rusqlite::ErrorCode::ConstraintViolation)
            {
                self.quarantine(op, &live, "publication validation failed")?;
            }
            return Err(e);
        }
        self.step(Phase::Published)
    }

    fn retire(&self, op: &Intent) -> Result<()> {
        let live = op.live(self);
        let trash = self.sessions().join(".trash").join(&op.incarnation);
        let dir = if live.try_exists()? { &live } else { &trash };
        // Once the verified trash is released for unlink, a crash can leave
        // only some of its files. That authorization never applies to live.
        let cleanup_authorized = op
            .metadata
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()?
            .and_then(|value| value["cleanup_authorized"].as_bool())
            == Some(true);
        if (dir == &live || !cleanup_authorized) && dir.try_exists()? {
            let verify = || -> Result<()> {
                let path = dir.join("session.db");
                if !path.try_exists()? {
                    return Err(layout::InvalidFormat("Missing retiring session.db".into()).into());
                }
                let _permit = Permits::take(1);
                let conn = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                layout::verify(&conn, SESSION)?;
                layout::integrity(&conn)?;
                verify_manifest(&conn, &op.store, &op.id, &op.incarnation)
            };
            if let Err(error) = verify() {
                if damaged_shard(&error) {
                    return self.quarantine(
                        op,
                        dir,
                        &format!("invalid retiring database: {error:#}"),
                    );
                }
                return Err(error);
            }
        }
        if live.try_exists()? {
            ensure!(!trash.try_exists()?, "Retired destination occupied");
            layout::rename(&live, &trash)?;
            self.step(Phase::TrashRenamed)?;
        }
        sync_dir(&self.sessions())?;
        sync_dir(trash.parent().unwrap())?;
        self.step(Phase::Trashed)?;
        if trash.try_exists()? {
            if !cleanup_authorized {
                self.global().write("authorize trash cleanup", &op.id, true, |tx| {
                    tx.execute("UPDATE lifecycle SET metadata=json_set(COALESCE(metadata,'{}'),'$.cleanup_authorized',json('true')) WHERE operation=?1",
                        [&op.operation])?;
                    Ok(())
                })?;
            }
            self.step(Phase::BeforeTrashRemoval)?;
            fs::remove_dir_all(&trash)?;
        }
        sync_dir(trash.parent().unwrap())?;
        self.step(Phase::Removed)?;
        self.global()
            .write("finish delete", &op.id, true, |tx| op.finish(tx))?;
        self.step(Phase::Retired)
    }

    pub(super) fn reconcile(&self) -> Result<()> {
        let keys = self.global().read(|c| {
            let mut q = c.prepare("SELECT storage_key FROM lifecycle WHERE quarantine_reason IS NULL ORDER BY storage_key")?;
            let rows = q.query_map([],|r|r.get::<_,String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for key in keys {
            ensure!(layout::is_storage_key(&key), "Invalid journal storage key");
            let _locks = self.key_lock(&key, true, false)?;
            if let Err(error) = self.recover_key(&key) {
                // Quarantined operations are terminal. Windows sharing errors
                // stay journaled and can be retried independently of other IDs.
                if self.pending(&key)?.is_some() && !sharing_violation(&error) {
                    return Err(error);
                }
            }
        }
        self.quarantine_orphans()
    }

    fn move_proven_orphan(&self, dir: &Path, name: &str, staged: bool) -> Result<()> {
        let path = dir.join("session.db");
        let inspect = || -> Result<()> {
            let _permit = Permits::take(1);
            ensure!(
                fs::symlink_metadata(&path)?.is_file(),
                "not a regular session file"
            );
            let identity = |c: &Connection| -> Result<()> {
                let (store,id,incarnation,seal): (String,String,String,String) = c.query_row(
                    "SELECT store_uuid,id,incarnation,sealed_operation FROM manifest WHERE singleton=1",
                    [], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
                ensure!(
                    store == self.global().uuid
                        && canonical_uuid(&incarnation)
                        && canonical_uuid(&seal)
                        && if staged {
                            seal == name
                        } else {
                            storage_key(&id)? == name
                        },
                    "orphan identity does not match this store and directory"
                );
                Ok(())
            };
            // A read-only SQLite WAL open can create or modify a shared-memory
            // sidecar. Inspect a nonempty WAL with a private WAL index, even
            // when the only copy of the sealed manifest is in that WAL.
            let wal = dir.join("session.db-wal");
            if wal.try_exists()? {
                let meta = fs::symlink_metadata(&wal)?;
                ensure!(meta.is_file(), "not a regular WAL");
                if meta.len() > 0 {
                    let scratch = tempfile::Builder::new()
                        .prefix(INSPECTION_PREFIX)
                        .tempdir_in(self.sessions().join(".quarantine"))?;
                    let owner = scratch.path().join("store-uuid");
                    fs::write(&owner, &self.global().uuid)?;
                    fs::File::options().write(true).open(owner)?.sync_all()?;
                    sync_dir(scratch.path())?;
                    let copy = scratch.path().join("session.db");
                    fs::copy(&path, &copy)?;
                    fs::copy(wal, scratch.path().join("session.db-wal"))?;
                    let conn = layout::open(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    layout::verify(&conn, SESSION)?;
                    identity(&conn)?;
                    drop(conn);
                    scratch.close()?;
                    return Ok(());
                }
            }
            // An immutable probe never creates sidecars in a foreign directory.
            let mut url = reqwest::Url::from_file_path(&path)
                .map_err(|_| anyhow::anyhow!("Invalid database path"))?;
            url.set_query(Some("immutable=1"));
            let probe = Connection::open_with_flags(
                url.as_str(),
                OpenFlags::SQLITE_OPEN_READ_ONLY
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            let app: i32 = probe.pragma_query_value(None, "application_id", |r| r.get(0))?;
            ensure!(app == SESSION, "unrecognized application ID");
            identity(&probe)?;
            drop(probe);
            let conn = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            layout::verify(&conn, SESSION)?;
            identity(&conn)
        };
        if let Err(error) = inspect() {
            tracing::warn!(path = %dir.display(), reason = %error, "Leaving unverified session directory in place");
            return Ok(());
        }
        durable_move(
            dir,
            &self
                .sessions()
                .join(".quarantine")
                .join(Uuid::new_v4().to_string()),
        )
    }

    fn quarantine_orphans(&self) -> Result<()> {
        let _sweep =
            layout::lock_existing(&self.root.path.join("session-locks/.orphan-sweep"), true)?;
        let quarantine = self.sessions().join(".quarantine");
        // The sweep lock also serializes cleanup after a killed WAL inspection.
        let mut has_quarantine = false;
        for entry in fs::read_dir(&quarantine)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(INSPECTION_PREFIX)
            {
                if entry.file_type()?.is_dir()
                    && owned_inspection(&entry.path(), &self.global().uuid)?
                {
                    fs::remove_dir_all(entry.path())?;
                    continue;
                }
                tracing::warn!(path = %entry.path().display(), "Leaving unverified inspection directory in place");
            }
            has_quarantine = true;
        }
        // Unjournaled orphans have no G intent to drive replay after rename.
        // A surviving destination can come from either the live or staging parent.
        if has_quarantine {
            sync_dir(&quarantine)?;
            sync_dir(&self.sessions())?;
            sync_dir(&self.sessions().join(".staging"))?;
        }
        for entry in fs::read_dir(self.sessions().join(".staging"))? {
            let entry = entry?;
            let operation = entry.file_name();
            let Some(operation) = operation.to_str().filter(|s| canonical_uuid(s)) else {
                tracing::warn!(path = %entry.path().display(), "Leaving unrecognized staging entry in place");
                continue;
            };
            // An operation's UUID is never reused. Its intent must already be durable
            // before its stage exists, so no live builder can own an unjournaled stage.
            let known = self.global().read(|c| Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM lifecycle WHERE operation=?1 AND quarantine_reason IS NULL)",
                [operation], |r| r.get::<_, bool>(0))?))?;
            if !known && entry.file_type()?.is_dir() {
                self.move_proven_orphan(&entry.path(), operation, true)?;
            } else if !known {
                tracing::warn!(path = %entry.path().display(), "Leaving unrelated staging entry in place");
            }
        }
        for entry in fs::read_dir(self.sessions())? {
            let entry = entry?;
            let key = entry.file_name();
            if matches!(key.to_str(), Some(".staging" | ".trash" | ".quarantine")) {
                continue;
            }
            let Some(key) = key.to_str().filter(|k| layout::is_storage_key(k)) else {
                tracing::warn!(path = %entry.path().display(), "Leaving unrecognized sessions entry in place");
                continue;
            };
            if !entry.file_type()?.is_dir() {
                tracing::warn!(path = %entry.path().display(), "Leaving unrelated sessions entry in place");
                continue;
            }
            let owned = || {
                self.global().read(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE storage_key=?1
                UNION ALL SELECT 1 FROM lifecycle WHERE storage_key=?1 AND quarantine_reason IS NULL)",[key],|r|r.get::<_,bool>(0))?))
            };
            if owned()? {
                continue;
            }
            let _locks = self.key_lock(key, true, false)?;
            if entry.path().try_exists()? && !owned()? {
                self.move_proven_orphan(&entry.path(), key, false)?;
            }
        }
        Ok(())
    }
}

const INSPECTION_PREFIX: &str = ".orphan-inspection-";

fn owned_inspection(dir: &Path, store: &str) -> Result<bool> {
    let marker = dir.join("store-uuid");
    if !fs::symlink_metadata(&marker).is_ok_and(|m| m.is_file() && m.len() == store.len() as u64)
        || !fs::read(marker).is_ok_and(|contents| contents == store.as_bytes())
    {
        return Ok(false);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file()
            || !matches!(
                entry.file_name().to_str(),
                Some("store-uuid" | "session.db" | "session.db-wal" | "session.db-shm")
            )
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn damaged_shard(error: &anyhow::Error) -> bool {
    if let Some(rusqlite::Error::SqliteFailure(_, Some(message))) = error.downcast_ref() {
        if [
            "no such table:",
            "no such column:",
            "foreign key mismatch",
            "malformed database schema",
        ]
        .iter()
        .any(|prefix| message.starts_with(prefix))
        {
            return true;
        }
    }
    error.is::<layout::InvalidFormat>()
        || matches!(
            error.downcast_ref::<rusqlite::Error>(),
            Some(
                rusqlite::Error::QueryReturnedNoRows
                    | rusqlite::Error::InvalidColumnType(..)
                    | rusqlite::Error::FromSqlConversionFailure(..)
            ) | Some(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase,
                    ..
                },
                _
            ))
        )
}

fn sharing_violation(e: &anyhow::Error) -> bool {
    cfg!(windows)
        && e.downcast_ref::<std::io::Error>()
            .is_some_and(|e| matches!(e.raw_os_error(), Some(32 | 33)))
}

impl SessionDb {
    pub fn delete(&self) -> Result<()> {
        let s = &self.stores;
        let _locks = s.key_lock(&self.key, true, false)?;
        self.check_directory()?;
        self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |_| Ok(()))?;
        let op = Intent::new(s, &self.id, &self.key, self.incarnation.clone(), "delete");
        s.global().write("retire session", &self.id, true, |tx| {
            ensure!(
                tx.execute(
                    "DELETE FROM conversations WHERE id=?1 AND incarnation=?2",
                    [&self.id, &self.incarnation]
                )? == 1,
                "Stale session handle"
            );
            op.insert(tx, None)?;
            s.step(Phase::BeforeDeleteIntentCommit)
        })?;
        s.step(Phase::DeleteIntent)?;
        s.retire(&op)
    }
}
