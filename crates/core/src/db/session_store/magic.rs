//! Project-scoped conversation excerpts used by Magic Docs.
use super::{DirectoryFilter, Stores};
use crate::db::{project_repo, DbManager};
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
pub struct Excerpt {
    session: String,
    role: String,
    pub content: String,
    #[serde(skip)]
    at: String,
    #[serde(skip)]
    id: String,
}

pub trait MagicHistory: super::Projects {
    fn conversation_consent(&self, project: &str) -> Result<bool>;
    fn configure_conversations(&self, project: &str, workspace: &Path, enabled: bool)
        -> Result<()>;
    fn conversation_excerpts(
        &self,
        project: &str,
        workspace: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Excerpt>>;
}

fn trusted(c: &Connection, id: &str, workspace: &Path) -> Result<()> {
    let project = project_repo::require_trusted(c, id)?;
    ensure!(
        !project.is_archived
            && project
                .workspace_path
                .as_deref()
                .and_then(|p| std::fs::canonicalize(p).ok())
                .as_deref()
                == Some(workspace),
        "Project does not match this workspace"
    );
    Ok(())
}

fn excerpts(c: &Connection, select: &str, id: &str) -> Result<Vec<Excerpt>> {
    let mut stmt = c.prepare_cached(select)?;
    let rows = stmt.query_map([id], |r| {
        Ok(Excerpt {
            session: r.get(0)?,
            role: r.get(1)?,
            content: r.get(2)?,
            at: r.get(3)?,
            id: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
fn key(project: &str) -> String {
    format!("magic-docs-conversations:{project}")
}

impl MagicHistory for DbManager {
    fn conversation_consent(&self, project: &str) -> Result<bool> {
        self.with_conn(|c| consent(c, project, false))?
    }
    fn configure_conversations(
        &self,
        project: &str,
        workspace: &Path,
        enabled: bool,
    ) -> Result<()> {
        self.with_conn(|c| crate::db::write_tx(c,"save project conversation consent",crate::db::STARTUP_WRITE_DEADLINE,|tx|{
            trusted(tx,project,workspace)?;
            tx.execute("INSERT INTO peripheral_settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key(project),enabled.to_string()])?;
            Ok(())
        }))?
    }
    fn conversation_excerpts(
        &self,
        project: &str,
        workspace: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Excerpt>> {
        ensure!(!cancelled(), "Conversation search cancelled");
        self.with_conn(|c|{
            let tx=c.unchecked_transaction()?;
            trusted(&tx,project,workspace)?;
            if !consent(&tx,project,false)? {return Ok(Vec::new());}
            excerpts(&tx,"SELECT m.conversation_id,m.role,substr(m.content,1,2000),m.created_at,m.id FROM messages m JOIN conversations c ON c.id=m.conversation_id WHERE c.project_id=?1 AND c.archived=0 AND m.role IN ('user','assistant') ORDER BY m.created_at DESC,m.id DESC LIMIT 12",project)
        })?
    }
}
fn consent(c: &Connection, project: &str, split: bool) -> Result<bool> {
    let sql = if split {
        "SELECT value FROM global_settings WHERE domain='project' AND name=?1"
    } else {
        "SELECT value FROM peripheral_settings WHERE key=?1"
    };
    let raw: Option<String> = c.query_row(sql, [key(project)], |r| r.get(0)).optional()?;
    Ok(raw
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or(false))
}
impl MagicHistory for Stores {
    fn conversation_consent(&self, project: &str) -> Result<bool> {
        self.global().read(|c| consent(c, project, true))
    }
    fn configure_conversations(
        &self,
        project: &str,
        workspace: &Path,
        enabled: bool,
    ) -> Result<()> {
        self.global().write("save project conversation consent",project,false,|c|{
            trusted(c,project,workspace)?;
            c.execute("INSERT INTO global_settings VALUES('project',?1,?2) ON CONFLICT(domain,name) DO UPDATE SET value=excluded.value",params![key(project),enabled.to_string()])?;
            Ok(())
        })
    }
    fn conversation_excerpts(
        &self,
        project: &str,
        workspace: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Excerpt>> {
        let selected = self.global().read(|c| {
            let tx = c.unchecked_transaction()?;
            trusted(&tx, project, workspace)?;
            if !consent(&tx, project, true)? {
                return Ok(Vec::new());
            }
            super::directory::list(
                &tx,
                DirectoryFilter {
                    project: Some(project),
                    archived: Some(false),
                    ..Default::default()
                },
            )
        })?;
        let mut latest = Vec::new();
        for entry in selected {
            ensure!(!cancelled(), "Conversation search cancelled");
            let shard = self.open_listed(&entry)?;
            let rows=shard.read(|r| excerpts(r.0,"SELECT conversation_id,role,substr(content,1,2000),created_at,id FROM messages WHERE conversation_id=?1 AND role IN ('user','assistant') ORDER BY created_at DESC,id DESC LIMIT 12",shard.id()))?;
            latest.extend(rows.into_iter().map(|row| (row, entry.incarnation.clone())));
            latest.sort_by(|(a, _), (b, _)| {
                (&b.at, &b.id, &b.session).cmp(&(&a.at, &a.id, &a.session))
            });
            latest.truncate(12);
        }
        // Consent, membership, archive state or incarnation can change during the scan.
        self.global().read(|c|{
            let tx=c.unchecked_transaction()?;
            trusted(&tx,project,workspace)?;
            if !consent(&tx,project,true)? {return Ok(Vec::new());}
            let mut result=Vec::new();
            let mut stmt=tx.prepare_cached("SELECT 1 FROM conversations WHERE id=?1 AND incarnation=?2 AND project_id=?3 AND archived=0")?;
            for (row,incarnation) in latest {
                if stmt.exists(params![row.session,incarnation,project])? {result.push(row);}
            }
            Ok(result)
        })
    }
}
