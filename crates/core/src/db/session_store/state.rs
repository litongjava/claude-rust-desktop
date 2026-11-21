//! Ownership-specific settings. S methods always participate in the caller's transaction.
use super::*;
use crate::permissions::PermissionMode;
use crate::services::{
    goal_proposals, permission_authority, session_goals, session_permission_mode,
};
use rusqlite::params;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
pub enum GlobalSetting<'a> {
    MachinePreference(&'a str),
    AgentDefinition(&'a str),
    LearningRecord(&'a str),
    ProjectConfiguration(&'a str),
    TeamConfiguration(&'a str),
}
impl<'a> GlobalSetting<'a> {
    pub(super) fn parts(self) -> [&'a str; 2] {
        match self {
            Self::MachinePreference(k) => ["machine", k],
            Self::AgentDefinition(k) => ["agent", k],
            Self::LearningRecord(k) => ["learning", k],
            Self::ProjectConfiguration(k) => ["project", k],
            Self::TeamConfiguration(k) => ["team", k],
        }
    }
}
impl GlobalDb {
    pub fn setting<T: DeserializeOwned>(&self, key: GlobalSetting<'_>) -> Result<Option<T>> {
        layout::allow_entry()?;
        let raw: Option<String> = self.read(|c| {
            Ok(c.query_row(
                "SELECT value FROM global_settings WHERE domain=?1 AND name=?2",
                key.parts(),
                |r| r.get(0),
            )
            .optional()?)
        })?;
        raw.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }
    pub fn set_setting<T: Serialize>(&self, key: GlobalSetting<'_>, value: &T) -> Result<()> {
        layout::allow_entry()?;
        let [domain, name] = key.parts();
        let raw = serde_json::to_string(value)?;
        self.write("save global setting",name,false,|c|{
            c.execute("INSERT INTO global_settings VALUES(?1,?2,?3) ON CONFLICT(domain,name) DO UPDATE SET value=excluded.value WHERE value<>excluded.value",[domain,name,&raw])?;
            Ok(())
        })
    }
    pub fn summary(&self, id: &str) -> Result<Option<Projection>> {
        self.read_directory_row(id, |c| {
            Ok(c.query_row(
                "SELECT content_revision,message_count,updated_at,last_turn_status,first_prompt,latest_prompt,
                 transcript_bytes,git_branch,request_count,input_tokens,output_tokens,total_tokens FROM conversations WHERE id=?1",
                [id], Projection::from_row,
            ).optional()?)
        })
    }
}

pub(super) fn local(c: &Connection, key: &str) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT value FROM peripheral_settings WHERE key=?1",
        [key],
        |r| r.get(0),
    )
    .optional()?)
}
pub(super) fn set_local(c: &Connection, key: &str, value: &str) -> Result<()> {
    c.execute("INSERT INTO peripheral_settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value WHERE value<>excluded.value",[key,value])?;
    Ok(())
}
fn session_id(c: &Connection) -> Result<String> {
    Ok(c.query_row("SELECT id FROM manifest", [], |r| r.get(0))?)
}

impl SessionRead<'_> {
    pub fn goal(&self) -> Result<Option<session_goals::Goal>> {
        session_goals::active_conn(self.0, &session_id(self.0)?)
    }
    pub fn goal_proposals(&self) -> Result<goal_proposals::ProposalState> {
        goal_proposals::read(self.0, &session_id(self.0)?)
    }
    pub fn agent_mode(&self) -> Result<Option<PermissionMode>> {
        local(
            self.0,
            &format!("cli.permission-mode.{}", session_id(self.0)?),
        )?
        .map(|s| session_permission_mode::parse(&s))
        .transpose()
    }
    pub fn plan_returns_to_bypass(&self, bypass_available: bool) -> Result<bool> {
        Ok(bypass_available
            && self.agent_mode()? == Some(PermissionMode::PlanMode)
            && local(
                self.0,
                &format!("plan.return-bypass.{}", session_id(self.0)?),
            )?
            .as_deref()
                == Some("true"))
    }
    pub fn git_branch(&self) -> Result<Option<String>> {
        local(self.0, "git.branch")
    }
    pub fn approved_plan_boundary(&self) -> Result<Option<String>> {
        Ok(self.0.query_row("SELECT m.id FROM messages m JOIN approved_plan_boundaries b ON b.message_id=m.id ORDER BY sort_order DESC,m.rowid DESC LIMIT 1",[],|r|r.get(0)).optional()?)
    }
    pub fn message_metadata(&self, id: &str) -> Result<Option<Value>> {
        read_metadata(self.0, "message_metadata", "message_id", id)
    }
    pub fn request_metadata(&self, id: &str) -> Result<Option<Value>> {
        read_metadata(self.0, "request_metadata", "request_id", id)
    }
}
impl SessionWrite<'_> {
    pub fn set_goal(&self, condition: &str) -> Result<session_goals::Goal> {
        session_goals::set_conn(self.0, self.1, condition)
    }
    pub fn clear_goal(&self) -> Result<bool> {
        session_goals::clear_conn(self.0, self.1)
    }
    pub fn save_goal_proposals(
        &self,
        expected: &goal_proposals::ProposalState,
        state: &goal_proposals::ProposalState,
    ) -> Result<()> {
        ensure!(
            goal_proposals::read(self.0, self.1)? == *expected,
            "Goal proposal state changed"
        );
        goal_proposals::save_state(self.0, self.1, state)
    }
    pub fn set_agent_mode(&self, mode: PermissionMode) -> Result<()> {
        self.set_agent_mode_from(mode, PermissionMode::AskPermissions, true)
    }
    /// Include the host's effective mode and capability when they differ from persisted state.
    pub fn set_agent_mode_from(
        &self,
        mode: PermissionMode,
        effective: PermissionMode,
        bypass_available: bool,
    ) -> Result<()> {
        save_agent_mode(self.0,self.1,mode,effective,bypass_available)
    }
    pub fn set_git_branch(&self, branch: Option<&str>) -> Result<()> {
        match branch {
            Some(name) => set_local(self.0, "git.branch", name)?,
            None => {
                self.0
                    .execute("DELETE FROM peripheral_settings WHERE key='git.branch'", [])?;
            }
        }
        self.2.set(true);
        Ok(())
    }
    pub fn approve_plan(&self, content: &str) -> Result<String> {
        // The legacy plan setting refers to the latest approval only. A rewind
        // removing that marker must never reactivate an older approval.
        self.0.execute("DELETE FROM approved_plan_boundaries", [])?;
        let message = self.append("user", content)?;
        self.0.execute(
            "UPDATE messages SET is_compact_boundary=1 WHERE id=?1",
            [&message],
        )?;
        self.0.execute(
            "INSERT INTO approved_plan_boundaries VALUES(?1)",
            [&message],
        )?;
        permission_authority::record_conn(
            self.0,
            self.1,
            permission_authority::Source::ApprovedPlan,
            &Value::String(content.into()),
        )?;
        Ok(message)
    }
    pub fn set_message_metadata(&self, id: &str, value: &Value) -> Result<()> {
        save_metadata(self.0, "message_metadata", "message_id", id, value)
    }
    pub fn set_request_metadata(&self, id: &str, value: &Value) -> Result<()> {
        save_metadata(self.0, "request_metadata", "request_id", id, value)
    }
}
fn read_metadata(c: &Connection, table: &str, key: &str, id: &str) -> Result<Option<Value>> {
    let raw: Option<String> = c
        .query_row(
            &format!("SELECT value FROM {table} WHERE {key}=?1"),
            [id],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
}
fn save_metadata(c: &Connection, table: &str, key: &str, id: &str, value: &Value) -> Result<()> {
    c.execute(&format!("INSERT INTO {table} VALUES(?1,?2) ON CONFLICT({key}) DO UPDATE SET value=excluded.value WHERE value<>excluded.value"),params![id,serde_json::to_string(value)?])?;
    Ok(())
}

fn stamp(metadata: std::fs::Metadata) -> Result<(String, String)> {
    Ok((
        metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
            .to_string(),
        metadata.len().to_string(),
    ))
}
impl SessionDb {
    /// Capture evidence for exactly the bytes returned to the caller.
    pub fn read_tracked_file(&self, path: &Path) -> Result<Vec<u8>> {
        self.render_tracked_file(path, Ok)
    }
    pub(crate) fn render_tracked_file<T>(
        &self,
        path: &Path,
        render: impl FnOnce(Vec<u8>) -> Result<T>,
    ) -> Result<T> {
        use std::io::Read;
        layout::allow_entry()?;
        let path = path.canonicalize()?;
        let mut file = std::fs::File::open(&path)?;
        let before = stamp(file.metadata()?)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        ensure!(
            before == stamp(file.metadata()?)? && before == stamp(path.metadata()?)?,
            "File changed during read"
        );
        let digest = Sha256::digest(&bytes);
        let result = render(bytes)?;
        self.write_named("record file read",|w|{
            w.0.execute("INSERT INTO file_read_stamps(path,modified_ns,size,digest) VALUES(?1,?2,?3,?4) ON CONFLICT(path) DO UPDATE SET modified_ns=excluded.modified_ns,size=excluded.size,digest=excluded.digest WHERE modified_ns<>excluded.modified_ns OR size<>excluded.size OR digest IS NOT excluded.digest",
                params![path.as_os_str().as_encoded_bytes(),before.0,before.1,&digest[..]])?;
            Ok(())
        })?;
        Ok(result)
    }
    pub fn check_file_write(&self, path: &Path) -> Result<()> {
        layout::allow_entry()?;
        let path = match path.canonicalize() {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return self.read(|_| Ok(())),
            Err(e) => return Err(e.into()),
        };
        let (modified, size, digest): (String, String, Option<Vec<u8>>) = self.read(|r| {
            r.0.query_row(
                "SELECT modified_ns,size,digest FROM file_read_stamps WHERE path=?1",
                [path.as_os_str().as_encoded_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .context("File has not been read yet")
        })?;
        let expected = (modified, size);
        let mut file = std::fs::File::open(&path)?;
        ensure!(
            expected == stamp(file.metadata()?)?,
            "File has been modified since read"
        );
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash)?;
        ensure!(
            digest.as_deref() == Some(&hash.finalize()[..])
                && expected == stamp(file.metadata()?)?
                && expected == stamp(path.metadata()?)?,
            "File has been modified since read"
        );
        Ok(())
    }
}

pub(super) fn save_agent_mode(c:&Connection,session:&str,mode:PermissionMode,effective:PermissionMode,bypass_available:bool)->Result<()> {
        let key = format!("cli.permission-mode.{}", session);
        if mode == PermissionMode::PlanMode {
            let previous = local(c, &key)?;
            if previous.as_deref() != Some("plan") {
                let bypass = bypass_available
                    && (effective == PermissionMode::BypassPermissions
                        || previous.as_deref() == Some("bypass"));
                set_local(
                    c,
                    &format!("plan.return-bypass.{}", session),
                    if bypass { "true" } else { "false" },
                )?;
            }
        }
        set_local(c, &key, session_permission_mode::name(mode))
}
