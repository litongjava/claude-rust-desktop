//! Directory registrations spanning an IPC wait retain both session identities.
use super::{SessionAccess, Stores};
use crate::{
    db::{conversation_repo::ConversationRow, DbManager},
    native_engine::child_session::{ChildOwnership, OWNER_KEY_PREFIX},
    runtime::SessionStore,
    services::session_parking::ParkedSession,
};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct WorkerPair {
    parent: String,
    child: String,
    identities: Option<(String, String, String)>,
}

pub trait WorkerDirectory: SessionAccess {
    fn worker_pair(&self, parent: &str, child: &str) -> Result<WorkerPair>;
    fn save_child_owner(&self, pair: &WorkerPair, record: &ChildOwnership) -> Result<()>;
    fn child_owner(&self, child: &str) -> Result<Option<ChildOwnership>>;
    fn park_worker(&self, pair: &WorkerPair, record: &ParkedSession) -> Result<()>;
    fn parked_workers(&self, parent: &str) -> Result<Vec<ParkedSession>>;
    fn parked_metadata(&self, id: &str) -> Result<Option<ConversationRow>>;
    fn parking_workspace(
        self: &Arc<Self>,
        parent: &str,
        child: &str,
        root: &Path,
    ) -> Result<PathBuf>;
}

impl WorkerPair {
    fn verify<'a>(&'a self, stores: &Stores, tx: &Connection) -> Result<(&'a str, &'a str)> {
        let (store, parent, child) = self
            .identities
            .as_ref()
            .context("Worker registration lacks a shard identity")?;
        ensure!(
            store == stores.global().store_uuid(),
            "Worker data root changed"
        );
        let current: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM conversations p, conversations c
            WHERE p.id=?1 AND c.id=?2 AND p.incarnation=?3 AND c.incarnation=?4 AND p.archived=0 AND c.archived=0)",
            params![self.parent,self.child,parent,child], |r| r.get(0))?;
        ensure!(
            current,
            "Worker session retired or archived during registration"
        );
        Ok((parent, child))
    }
}

impl WorkerDirectory for Stores {
    fn worker_pair(&self, parent: &str, child: &str) -> Result<WorkerPair> {
        let (p,c) = self.global().read(|conn| Ok(conn.query_row("SELECT p.incarnation,c.incarnation
            FROM conversations p,conversations c WHERE p.id=?1 AND c.id=?2 AND p.archived=0 AND c.archived=0",
            [parent,child], |r| Ok((r.get(0)?,r.get(1)?)))?))?;
        Ok(WorkerPair {
            parent: parent.into(),
            child: child.into(),
            identities: Some((self.global().uuid.clone(), p, c)),
        })
    }
    fn save_child_owner(&self, pair: &WorkerPair, record: &ChildOwnership) -> Result<()> {
        ensure!(
            record.parent_session_id == pair.parent && pair.parent != pair.child,
            "Child ownership mismatch"
        );
        let encoded = serde_json::to_string(record)?;
        self.global().write("record child ownership", &pair.child, false, |tx| {
            let (p,c) = pair.verify(self,tx)?;
            let rows = tx.execute("INSERT INTO child_owners(child_incarnation,parent_incarnation,record) VALUES(?1,?2,?3)
                ON CONFLICT(child_incarnation) DO UPDATE SET record=excluded.record WHERE parent_incarnation=excluded.parent_incarnation",
                params![c,p,encoded])?;
            ensure!(rows == 1, "Child belongs to another owner");
            Ok(())
        })
    }
    fn child_owner(&self, child: &str) -> Result<Option<ChildOwnership>> {
        let raw: Option<(String,Option<String>)> = self.global().read(|c| Ok(c.query_row("SELECT o.record,p.id FROM conversations child
            JOIN child_owners o ON o.child_incarnation=child.incarnation LEFT JOIN conversations p ON p.incarnation=o.parent_incarnation
            WHERE child.id=?1", [child], |r| Ok((r.get(0)?,r.get(1)?))).optional()?))?;
        raw.map(|(record, parent)| {
            let owner: ChildOwnership = serde_json::from_str(&record)?;
            ensure!(
                parent.as_deref() == Some(&owner.parent_session_id),
                "Child's recorded parent is unavailable"
            );
            Ok(owner)
        })
        .transpose()
    }
    fn park_worker(&self, pair: &WorkerPair, record: &ParkedSession) -> Result<()> {
        ensure!(
            record.parent_session_id == pair.parent && record.session_id == pair.child,
            "Parked session ownership mismatch"
        );
        self.global().write("register parked session", &pair.child, false, |tx| {
            let (p,c)=pair.verify(self,tx)?;
            tx.execute("INSERT INTO parked_sessions(parent_incarnation,child_incarnation,initial_worker_id,created_at)
                VALUES(?1,?2,?3,?4) ON CONFLICT(parent_incarnation,child_incarnation)
                DO UPDATE SET initial_worker_id=excluded.initial_worker_id,created_at=excluded.created_at",
                params![p,c,record.initial_worker_id,record.created_at])?;
            Ok(())
        })
    }
    fn parked_workers(&self, parent: &str) -> Result<Vec<ParkedSession>> {
        self.global().read(|c|{
            let mut q=c.prepare("SELECT p.id,c.id,s.initial_worker_id,s.created_at FROM conversations p
                JOIN parked_sessions s ON s.parent_incarnation=p.incarnation JOIN conversations c ON c.incarnation=s.child_incarnation
                WHERE p.id=?1 ORDER BY c.id")?;
            let rows=q.query_map([parent],|r|Ok(ParkedSession {parent_session_id:r.get(0)?,session_id:r.get(1)?,initial_worker_id:r.get(2)?,created_at:r.get(3)?}))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
    fn parked_metadata(&self, id: &str) -> Result<Option<ConversationRow>> {
        self.conversation_metadata(id)
    }
    fn parking_workspace(
        self: &Arc<Self>,
        parent: &str,
        child: &str,
        root: &Path,
    ) -> Result<PathBuf> {
        let store = SessionStore::new(self.clone());
        store.resume(parent)?;
        store.resume(child)?;
        store.workspace(child, root)
    }
}

fn save_shared(db: &DbManager, key: &str, encoded: &str) -> Result<()> {
    db.with_conn(|c|crate::db::write_tx(c,"save worker registration",crate::db::STARTUP_WRITE_DEADLINE,|tx|{
        tx.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[key,encoded])?;
        Ok(())
    }))?
}

impl WorkerDirectory for DbManager {
    fn worker_pair(&self, parent: &str, child: &str) -> Result<WorkerPair> {
        Ok(WorkerPair {
            parent: parent.into(),
            child: child.into(),
            identities: None,
        })
    }
    fn save_child_owner(&self, pair: &WorkerPair, record: &ChildOwnership) -> Result<()> {
        ensure!(
            record.parent_session_id == pair.parent,
            "Child ownership mismatch"
        );
        save_shared(
            self,
            &format!("{OWNER_KEY_PREFIX}{}", pair.child),
            &serde_json::to_string(record)?,
        )
    }
    fn child_owner(&self, child: &str) -> Result<Option<ChildOwnership>> {
        let raw: Option<String> = self.with_conn(|c| {
            c.query_row(
                "SELECT value FROM peripheral_settings WHERE key=?1",
                [format!("{OWNER_KEY_PREFIX}{child}")],
                |r| r.get(0),
            )
            .optional()
        })??;
        raw.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }
    fn park_worker(&self, pair: &WorkerPair, record: &ParkedSession) -> Result<()> {
        ensure!(
            record.parent_session_id == pair.parent && record.session_id == pair.child,
            "Parked session ownership mismatch"
        );
        save_shared(
            self,
            &format!("parked-session:{}:{}", pair.parent, pair.child),
            &serde_json::to_string(record)?,
        )
    }
    fn parked_workers(&self, parent: &str) -> Result<Vec<ParkedSession>> {
        let raw = self.with_conn(|c| -> rusqlite::Result<Vec<String>> {
            let mut q =
                c.prepare("SELECT value FROM peripheral_settings WHERE key LIKE ?1 ORDER BY key")?;
            let rows = q.query_map([format!("parked-session:{parent}:%")], |r| r.get(0))?;
            rows.collect()
        })??;
        raw.iter().map(|v| Ok(serde_json::from_str(v)?)).collect()
    }
    fn parked_metadata(&self, id: &str) -> Result<Option<ConversationRow>> {
        self.conversation_metadata(id)
    }
    fn parking_workspace(
        self: &Arc<Self>,
        parent: &str,
        child: &str,
        root: &Path,
    ) -> Result<PathBuf> {
        let store = SessionStore::new(self.clone());
        store.resume(parent)?;
        store.resume(child)?;
        store.workspace(child, root)
    }
}

impl Stores {
    pub(crate) fn child_jobs(&self, parent: &str) -> Result<Vec<(String, ChildOwnership)>> {
        let rows = self.global().read(|c| {
            let mut q = c.prepare(
                "SELECT child.id,owner.record,p.id FROM child_owners owner
                JOIN conversations child ON child.incarnation=owner.child_incarnation
                LEFT JOIN conversations p ON p.incarnation=owner.parent_incarnation
                WHERE json_extract(owner.record,'$.parent_session_id')=?1",
            )?;
            let rows = q.query_map([parent], |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        rows.into_iter()
            .map(|(id, raw, p)| {
                ensure!(
                    p.as_deref() == Some(parent),
                    "Descendant's recorded owner was retired"
                );
                Ok((id, serde_json::from_str(&raw)?))
            })
            .collect()
    }
}
impl DbManager {
    pub(crate) fn child_jobs(&self, parent: &str) -> Result<Vec<(String, ChildOwnership)>> {
        let rows = self.with_conn(|c| -> rusqlite::Result<Vec<(String, String)>> {
            let mut q = c.prepare(
                "SELECT key,value FROM peripheral_settings WHERE key LIKE 'child-owner.%'",
            )?;
            let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect()
        })??;
        let mut found = Vec::new();
        for (key, raw) in rows {
            let owner: ChildOwnership = serde_json::from_str(&raw)?;
            if owner.parent_session_id == parent {
                found.push((
                    key.strip_prefix(OWNER_KEY_PREFIX)
                        .context("Invalid child owner key")?
                        .into(),
                    owner,
                ));
            }
        }
        Ok(found)
    }
}

pub(crate) trait WorkspaceDirectory {
    fn workspace_sessions(&self) -> Result<Vec<(String, String)>>;
}
impl<D: super::shared::DirectoryRepository> WorkspaceDirectory for D {
    fn workspace_sessions(&self) -> Result<Vec<(String, String)>> {
        self.directory_read(|c| {
            let mut q = c.prepare(
                "SELECT id,workspace_path FROM conversations WHERE workspace_path IS NOT NULL",
            )?;
            let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}

impl Stores {
    pub(crate) fn register_background_agent(
        &self,
        job: &str,
        record: &serde_json::Value,
    ) -> Result<()> {
        let text = |key: &str| {
            record[key]
                .as_str()
                .context("Missing background registration field")
        };
        self.global().write("register background agent",job,false,|tx|{
            ensure!(tx.execute("INSERT INTO background_agents(job,parent_incarnation,child_incarnation,agent_type,description,started_at)
                SELECT ?1,p.incarnation,c.incarnation,?4,?5,?6 FROM child_owners o
                JOIN conversations p ON p.incarnation=o.parent_incarnation JOIN conversations c ON c.incarnation=o.child_incarnation
                WHERE p.id=?2 AND c.id=?3 AND json_extract(o.record,'$.job_id')=?1",params![job,text("parent")?,text("session_id")?,text("agent_type")?,text("description")?,text("started_at")?])?==1,"Background worker registration changed");
            Ok(())
        })
    }
    pub fn background_agents(
        &self,
        job: Option<&str>,
        child: Option<&str>,
    ) -> Result<Vec<(String, serde_json::Value)>> {
        self.global().read(|c|{
            let mut q=c.prepare("SELECT a.job,p.id,c.id,a.agent_type,a.description,a.started_at FROM background_agents a
                JOIN conversations p ON p.incarnation=a.parent_incarnation JOIN conversations c ON c.incarnation=a.child_incarnation
                WHERE (?1 IS NULL OR a.job=?1) AND (?2 IS NULL OR c.id=?2) ORDER BY a.started_at,a.job")?;
            let rows=q.query_map(params![job,child],|r|Ok((r.get(0)?,serde_json::json!({"parent":r.get::<_,String>(1)?,"session_id":r.get::<_,String>(2)?,"agent_type":r.get::<_,String>(3)?,"description":r.get::<_,String>(4)?,"started_at":r.get::<_,String>(5)?}))))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
    pub fn background_agent(&self, job: &str) -> Result<Option<serde_json::Value>> {
        Ok(self
            .background_agents(Some(job), None)?
            .pop()
            .map(|(_, r)| r))
    }
}
impl DbManager {
    pub(crate) fn register_background_agent(
        &self,
        job: &str,
        record: &serde_json::Value,
    ) -> Result<()> {
        save_shared(
            self,
            &format!("background-agent.{job}"),
            &serde_json::to_string(record)?,
        )
    }
    pub fn background_agents(
        &self,
        job: Option<&str>,
        child: Option<&str>,
    ) -> Result<Vec<(String, serde_json::Value)>> {
        let rows=self.with_conn(|c|->rusqlite::Result<Vec<(String,String)>>{
            let mut q=c.prepare("SELECT substr(key,18),value FROM peripheral_settings WHERE key LIKE 'background-agent.%' AND (?1 IS NULL OR key='background-agent.'||?1) ORDER BY rowid")?;
            let rows=q.query_map([job],|r|Ok((r.get(0)?,r.get(1)?)))?;rows.collect()
        })??;
        let mut result = Vec::new();
        for (job, raw) in rows {
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            if child.is_none_or(|c| value["session_id"] == c) {
                result.push((job, value));
            }
        }
        Ok(result)
    }
    pub fn background_agent(&self, job: &str) -> Result<Option<serde_json::Value>> {
        Ok(self
            .background_agents(Some(job), None)?
            .pop()
            .map(|(_, r)| r))
    }
}
