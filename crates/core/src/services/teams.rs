//! Persistent agent-team identity shared by hosts and the standalone CLI.
//!
//! Upstream stores `teams/<name>/config.json` under the config home with a
//! deterministic `name@team` agent ID per member. The leader is the session
//! that created the team; teammates are child sessions owned by this process.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const LEAD_NAME: &str = "team-lead";

/// Sessions currently executing in this process. Anything persisted as active
/// but absent here is stale after a crash or restart.
static LIVE: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn live() -> std::sync::MutexGuard<'static, Option<HashSet<String>>> {
    LIVE.lock().unwrap_or_else(|error| error.into_inner())
}

pub fn register_live(session: &str) {
    live()
        .get_or_insert_with(HashSet::new)
        .insert(session.into());
}

pub fn unregister_live(session: &str) {
    if let Some(set) = live().as_mut() {
        set.remove(session);
    }
}

pub fn is_live(session: &str) -> bool {
    live().as_ref().is_some_and(|set| set.contains(session))
}

/// Upstream also requires a `--agent-teams` flag and a remote killswitch. The
/// CLI flag sets this environment variable; there is no remote gate here.
pub fn enabled() -> bool {
    enabled_with(
        std::env::var("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS")
            .ok()
            .as_deref(),
    )
}

fn enabled_with(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub fn ensure_enabled() -> Result<()> {
    if enabled() {
        Ok(())
    } else {
        bail!("Agent teams are disabled; set CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 or pass --agent-teams")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TeamMember {
    pub agent_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub joined_at: i64,
    #[serde(default)]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub subscriptions: Vec<String>,
    /// False when idle, stopped or stale; absent/true while a turn is running.
    #[serde(default = "default_true")]
    pub is_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TeamFile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created_at: i64,
    pub lead_agent_id: String,
    pub lead_session_id: String,
    #[serde(default)]
    pub members: Vec<TeamMember>,
}

pub fn sanitize_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    sanitized.trim_matches('-').to_string()
}

pub fn sanitize_agent_name(name: &str) -> String {
    name.replace('@', "-")
}

pub fn agent_id(name: &str, team: &str) -> String {
    format!("{}@{}", sanitize_agent_name(name), team)
}

pub fn team_dir(root: &Path, name: &str) -> PathBuf {
    root.join("teams").join(sanitize_name(name))
}

pub fn team_path(root: &Path, name: &str) -> PathBuf {
    team_dir(root, name).join("config.json")
}

fn global_lock(root: &Path) -> Result<std::fs::File> {
    let directory = root.join("teams");
    std::fs::create_dir_all(&directory)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(".lock"))?;
    file.lock()?;
    Ok(file)
}

fn lock(root: &Path, name: &str) -> Result<std::fs::File> {
    let directory = team_dir(root, name);
    std::fs::create_dir_all(&directory)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("config.lock"))?;
    file.lock()?;
    Ok(file)
}

pub fn read_team(root: &Path, name: &str) -> Result<Option<TeamFile>> {
    match std::fs::read(team_path(root, name)) {
        Ok(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("Invalid team file for {name}")
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_team(root: &Path, team: &TeamFile) -> Result<()> {
    let directory = team_dir(root, &team.name);
    std::fs::create_dir_all(&directory)?;
    let temp = directory.join(format!("config.json.{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temp, serde_json::to_vec_pretty(team)?)?;
    std::fs::rename(&temp, team_path(root, &team.name))?;
    Ok(())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Marks persisted active members whose session no longer runs in this process
/// as stopped, so a restarted leader never reports live teammates.
fn recover(team: &mut TeamFile) -> bool {
    let mut changed = false;
    for member in &mut team.members {
        if member.name != LEAD_NAME
            && member.is_active
            && !member.session_id.as_deref().is_some_and(is_live)
        {
            member.is_active = false;
            member.status = Some("stale".into());
            changed = true;
        }
    }
    changed
}

pub fn read_recovered(root: &Path, name: &str) -> Result<Option<TeamFile>> {
    let _lock = lock(root, name)?;
    let Some(mut team) = read_team(root, name)? else {
        return Ok(None);
    };
    if recover(&mut team) {
        write_team(root, &team)?;
    }
    Ok(Some(team))
}

/// Scans for the team this session leads, recovering stale members first.
pub fn led_by(root: &Path, session: &str) -> Result<Option<TeamFile>> {
    let directory = root.join("teams");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(team) = read_recovered(root, &name)? else {
            continue;
        };
        if team.lead_session_id == session {
            return Ok(Some(team));
        }
    }
    Ok(None)
}

#[derive(Debug)]
pub struct CreateOutput {
    pub team: TeamFile,
    pub team_file_path: PathBuf,
}

pub fn create(
    root: &Path,
    session: &str,
    name: &str,
    description: Option<&str>,
    agent_type: Option<&str>,
    model: Option<&str>,
    cwd: &Path,
) -> Result<CreateOutput> {
    ensure_enabled()?;
    if name.trim().is_empty() {
        bail!("team_name is required for TeamCreate");
    }
    if sanitize_name(name).is_empty() {
        bail!("Team name must contain at least one letter or digit");
    }
    let _global = global_lock(root)?;
    if let Some(existing) = led_by(root, session)? {
        bail!(
            "Already leading team \"{}\". A leader can only manage one team at a time. Use TeamDelete to end the current team before creating a new one.",
            existing.name
        );
    }
    // Upstream generates a word slug on collision; a stable suffix keeps the
    // requested name recognizable.
    let mut final_name = sanitize_name(name);
    let mut attempt = 2;
    while team_path(root, &final_name).exists() {
        final_name = format!("{}-{attempt}", sanitize_name(name));
        attempt += 1;
        if attempt > 100 {
            bail!("Could not allocate a unique team name");
        }
    }
    let file = TeamFile {
        name: final_name.clone(),
        description: description.map(str::to_owned),
        created_at: now(),
        lead_agent_id: agent_id(LEAD_NAME, &final_name),
        lead_session_id: session.into(),
        members: vec![TeamMember {
            agent_id: agent_id(LEAD_NAME, &final_name),
            name: LEAD_NAME.into(),
            agent_type: Some(
                agent_type
                    .filter(|value| !value.is_empty())
                    .unwrap_or(LEAD_NAME)
                    .into(),
            ),
            model: model.map(str::to_owned),
            prompt: None,
            joined_at: now(),
            cwd: cwd.to_string_lossy().into_owned(),
            session_id: Some(session.into()),
            subscriptions: Vec::new(),
            is_active: false,
            status: Some("idle".into()),
            result: None,
        }],
    };
    let _lock = lock(root, &final_name)?;
    write_team(root, &file)?;
    Ok(CreateOutput {
        team_file_path: team_path(root, &final_name),
        team: file,
    })
}

#[derive(Debug)]
pub struct DeleteOutput {
    pub success: bool,
    pub message: String,
    pub team_name: Option<String>,
}

pub fn delete(root: &Path, session: &str) -> Result<DeleteOutput> {
    ensure_enabled()?;
    let _global = global_lock(root)?;
    let Some(mut team) = led_by(root, session)? else {
        return Ok(DeleteOutput {
            success: true,
            message: "No team name found, nothing to clean up".into(),
            team_name: None,
        });
    };
    let active: Vec<&str> = team
        .members
        .iter()
        .filter(|member| member.name != LEAD_NAME && member.is_active)
        .map(|member| member.name.as_str())
        .collect();
    if !active.is_empty() {
        return Ok(DeleteOutput {
            success: false,
            message: format!(
                "Cannot cleanup team with {} active member(s): {}. Use requestShutdown to gracefully terminate teammates first.",
                active.len(),
                active.join(", ")
            ),
            team_name: Some(team.name),
        });
    }
    // Persist the recovered state before deleting for crash forensics.
    let _lock = lock(root, &team.name)?;
    if recover(&mut team) {
        write_team(root, &team)?;
    }
    std::fs::remove_dir_all(team_dir(root, &team.name))?;
    Ok(DeleteOutput {
        success: true,
        message: format!(
            "Cleaned up directories and worktrees for team \"{}\"",
            team.name
        ),
        team_name: Some(team.name),
    })
}

pub struct MemberOutput {
    pub agent_id: String,
}

/// Registers a teammate before its first turn. The caller must own the team.
pub fn register_member(
    root: &Path,
    team_name: &str,
    session: &str,
    name: &str,
    agent_type: Option<&str>,
    model: Option<&str>,
    prompt: &str,
    cwd: &Path,
) -> Result<MemberOutput> {
    let _lock = lock(root, team_name)?;
    let mut team =
        read_team(root, team_name)?.with_context(|| format!("Team not found: {team_name}"))?;
    if team.lead_session_id != session {
        bail!("Only the team leader can spawn teammates");
    }
    if name.trim().is_empty() {
        bail!("Teammate name is required");
    }
    if name == LEAD_NAME {
        bail!("\"{LEAD_NAME}\" is reserved for the team leader");
    }
    let requested_id = agent_id(name, &team.name);
    if team
        .members
        .iter()
        .any(|member| member.name.eq_ignore_ascii_case(name) || member.agent_id == requested_id)
    {
        bail!("Teammate already exists: {name}");
    }
    let member = TeamMember {
        agent_id: agent_id(name, &team.name),
        name: name.into(),
        agent_type: agent_type.map(str::to_owned),
        model: model.map(str::to_owned),
        prompt: Some(prompt.into()),
        joined_at: now(),
        cwd: cwd.to_string_lossy().into_owned(),
        session_id: None,
        subscriptions: Vec::new(),
        is_active: true,
        status: Some("active".into()),
        result: None,
    };
    let agent = member.agent_id.clone();
    team.members.push(member);
    write_team(root, &team)?;
    Ok(MemberOutput { agent_id: agent })
}

/// Binds the child session ID once the executor exists.
pub fn bind_member_session(
    root: &Path,
    team_name: &str,
    agent: &str,
    session_id: &str,
) -> Result<()> {
    let _lock = lock(root, team_name)?;
    let mut team =
        read_team(root, team_name)?.with_context(|| format!("Team not found: {team_name}"))?;
    team.members
        .iter_mut()
        .find(|member| member.agent_id == agent)
        .with_context(|| format!("Teammate not found: {agent}"))?
        .session_id = Some(session_id.into());
    write_team(root, &team)?;
    Ok(())
}

pub fn finish_member(
    root: &Path,
    team_name: &str,
    agent: &str,
    status: &str,
    result: Option<&str>,
) -> Result<()> {
    let _lock = lock(root, team_name)?;
    let Some(mut team) = read_team(root, team_name)? else {
        return Ok(());
    };
    if let Some(member) = team
        .members
        .iter_mut()
        .find(|member| member.agent_id == agent)
    {
        member.is_active = false;
        member.status = Some(status.into());
        member.result = result.map(|value| {
            if value.len() > 4096 {
                format!("{}…", &value[..value.floor_char_boundary(4096)])
            } else {
                value.into()
            }
        });
        write_team(root, &team)?;
    }
    Ok(())
}

pub fn member(root: &Path, team_name: &str, name: &str) -> Result<Option<TeamMember>> {
    Ok(read_recovered(root, team_name)?.and_then(|team| {
        team.members
            .into_iter()
            .find(|member| member.name.eq_ignore_ascii_case(name))
    }))
}

#[cfg(test)]
mod tests;
